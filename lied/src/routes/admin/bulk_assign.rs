//! `/admin/orgs/{org}/collections/{id}/bulk-assign` — put one player on the
//! same part in every piece of a collection ("the 1st trumpet on the whole
//! March Book", issue #55).
//!
//! Preview, then confirm. The `GET` renders a picker of the parts that exist in
//! the collection plus a username field; once both are given it shows, piece by
//! piece, what would happen ([`bulk_assign::plan`]). The confirm form carries
//! exactly the `(item, voice)` pairs the preview showed as "will assign", so the
//! `POST` writes what the archivist saw rather than re-matching
//! ([`bulk_assign::apply`]); if the collection changed in between it refuses
//! with 409 and writes nothing. After a successful write the browser returns to
//! the same preview, where every assigned piece now reads "already plays it" —
//! the confirmation, and the way to extend the assignment after adding pieces.
//!
//! Gated like the rest of the collections section
//! ([`ConsoleCtx::can_build_collections`]: owner/archivist/conductor).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use maud::{html, Markup};
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::bulk_assign::{self, Outcome, PartInCollection, PartKey, PlanItem, VoiceRef};
use crate::domain::{membership, user};
use crate::routes::admin::arrangements::{audit_ctx, csrf_token};
use crate::routes::admin::collections::enter_collection;
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::admin::members::{field, multi};
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/orgs/:org_id/collections/:collection_id/bulk-assign",
        get(bulk_assign_page).post(apply_bulk_assign),
    )
}

fn error_page(ctx: &ConsoleCtx, status: StatusCode, message: &str) -> Response {
    console::error_page(ctx, Section::Collections, status, message)
}

fn page_url(org_id: Uuid, collection_id: Uuid) -> String {
    format!("/admin/orgs/{org_id}/collections/{collection_id}/bulk-assign")
}

/// The `part` form value: `<instrument-uuid>:<part-number>`, with an empty
/// number for an unnumbered part (`<uuid>:`).
fn encode_part(key: PartKey) -> String {
    match key.part_number {
        Some(n) => format!("{}:{n}", key.instrument_id),
        None => format!("{}:", key.instrument_id),
    }
}

fn parse_part(raw: &str) -> Option<PartKey> {
    let (instrument, number) = raw.split_once(':')?;
    let instrument_id = Uuid::parse_str(instrument).ok()?;
    let part_number = if number.is_empty() {
        None
    } else {
        Some(number.parse::<i16>().ok().filter(|n| *n >= 1)?)
    };
    Some(PartKey {
        instrument_id,
        part_number,
    })
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass through).
/// Usernames carry no charset rule, so the redirect back to the preview must
/// not assume one.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn part_label(part: &PartInCollection) -> String {
    match part.part_number {
        Some(n) => format!("{} — part {n}", part.instrument_name),
        None => format!("{} — no part number", part.instrument_name),
    }
}

fn voice_label(v: &VoiceRef) -> Markup {
    html! {
        (v.name)
        @if v.part_number.is_none() {
            " " span class="muted" { "(no part number)" }
        }
    }
}

fn outcome_cell(item: &PlanItem) -> Markup {
    html! {
        @match item.outcome {
            Outcome::Assign => {
                strong { "will assign " }
                @if let Some(v) = &item.voice { (voice_label(v)) }
                @if !item.also_holds.is_empty() {
                    br;
                    span class="error" {
                        "Already plays "
                        @for (i, other) in item.also_holds.iter().enumerate() {
                            @if i > 0 { ", " }
                            (other.name)
                        }
                        " in this piece — one person cannot play both at once."
                    }
                }
            }
            Outcome::AlreadyHeld => {
                span class="muted" { "already plays " }
                @if let Some(v) = &item.voice { (voice_label(v)) }
            }
            Outcome::NoMatch => {
                span class="muted" { "no such part in this piece" }
                @if !item.candidates.is_empty() {
                    br;
                    span class="muted" { "This piece has: " }
                    @for (i, v) in item.candidates.iter().enumerate() {
                        @if i > 0 { ", " }
                        (voice_label(v))
                    }
                    span class="muted" {
                        " — if one of these is the same part, give it the right part "
                        "number in the catalog, or assign it on the piece's Parts screen."
                    }
                }
            }
            Outcome::Ambiguous => {
                span class="error" { "more than one matching voice: " }
                @for (i, v) in item.candidates.iter().enumerate() {
                    @if i > 0 { ", " }
                    (voice_label(v))
                }
                span class="muted" { " — assign it on the piece's Parts screen." }
            }
            Outcome::Removed => {
                span class="muted" {
                    "arrangement removed from the catalog — restore it before assigning"
                }
            }
        }
    }
}

async fn bulk_assign_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    Query(pairs): Query<Vec<(String, String)>>,
    session: Session,
) -> Response {
    let (ctx, c) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };

    let parts = match bulk_assign::parts_in_collection(&state.db, collection_id).await {
        Ok(parts) => parts,
        Err(error) => {
            tracing::error!(%error, "failed to list the collection's parts");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the parts in this collection.",
            );
        }
    };
    // The datalist is a convenience — the field accepts any username — so a
    // failed lookup degrades to an empty list rather than an error page, but
    // it is logged, not swallowed.
    let member_usernames = match membership::member_usernames(
        &state.db,
        org_id,
        i64::from(state.config.max_page_size),
    )
    .await
    {
        Ok(names) => names,
        Err(error) => {
            tracing::error!(%error, "failed to list member usernames for the datalist");
            Vec::new()
        }
    };

    let raw_part = field(&pairs, "part").filter(|v| !v.is_empty());
    let username = field(&pairs, "username")
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let key = match raw_part.map(parse_part) {
        None => None,
        Some(Some(key)) => Some(key),
        Some(None) => return error_page(&ctx, StatusCode::BAD_REQUEST, "Choose a part."),
    };

    // Resolve the player and build the plan once both inputs are present.
    let mut status = StatusCode::OK;
    let mut problem: Option<&str> = None;
    // Only the public fields are rendered; the hash never leaves this handler.
    let mut preview: Option<(user::UserWithHash, Vec<PlanItem>)> = None;
    if let (Some(key), Some(username)) = (key, username) {
        match user::find_by_username(&state.db, username).await {
            Ok(Some(player)) => {
                match bulk_assign::plan(&state.db, collection_id, key, player.id).await {
                    Ok(plan) => preview = Some((player, plan)),
                    Err(error) => {
                        tracing::error!(%error, "failed to plan a bulk assignment");
                        return error_page(
                            &ctx,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Could not work out the assignment.",
                        );
                    }
                }
            }
            Ok(None) => {
                status = StatusCode::NOT_FOUND;
                problem = Some(
                    "No user with that username. They need an account before they can be \
                     assigned a part.",
                );
            }
            Err(error) => {
                tracing::error!(%error, "failed to look up the player");
                return error_page(
                    &ctx,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not look up that user.",
                );
            }
        }
    }

    let action = page_url(org_id, collection_id);
    let body = html! {
        h2 { "Assign a part across — " (c.name) }
        p class="muted" {
            "Puts one player on the same part in every piece of this collection. "
            "\"The same part\" means the same instrument and part number, as set on "
            "each voice in the catalog. Nothing is written until you confirm, and "
            "each piece's assignment can still be changed on its own afterwards."
        }

        datalist id="member-usernames" {
            @for name in &member_usernames { option value=(name); }
        }

        @if parts.is_empty() {
            p class="muted" {
                "No piece in this collection has any voices yet — add voices in the "
                "catalog first."
            }
        } @else {
            form method="get" action=(&action) {
                label {
                    "Part "
                    select name="part" required {
                        option value="" { "Choose a part…" }
                        @for part in &parts {
                            @let value = encode_part(PartKey {
                                instrument_id: part.instrument_id,
                                part_number: part.part_number,
                            });
                            option value=(value) selected[raw_part == Some(value.as_str())] {
                                (part_label(part)) " (in " (part.pieces) " "
                                @if part.pieces == 1 { "piece" } @else { "pieces" }
                                ")"
                            }
                        }
                    }
                }
                label {
                    "Player "
                    input type="text" name="username" list="member-usernames"
                          placeholder="username" value=[username] required;
                }
                button type="submit" { "Preview" }
            }
        }

        @if let Some(message) = problem {
            p class="error" { (message) }
        }

        @if let Some((player, plan)) = &preview {
            @let assign: Vec<&PlanItem> =
                plan.iter().filter(|i| i.outcome == Outcome::Assign).collect();
            @let count = |o: Outcome| plan.iter().filter(|i| i.outcome == o).count();
            h3 { "Preview for " (player.display_name) " " span class="muted" { "(" (player.username) ")" } }
            p {
                strong { (assign.len()) } " to assign · "
                (count(Outcome::AlreadyHeld)) " already held · "
                (count(Outcome::NoMatch)) " without this part · "
                (count(Outcome::Ambiguous)) " ambiguous · "
                (count(Outcome::Removed)) " removed"
            }
            table {
                thead { tr { th { "#" } th { "Piece" } th { "What happens" } } }
                tbody {
                    @for item in plan {
                        tr {
                            td { (item.index) }
                            td {
                                a href=(format!(
                                    "/admin/orgs/{org_id}/collections/{collection_id}/items/{}/assignments",
                                    item.collection_item_id
                                )) { (item.arrangement_title) }
                            }
                            td { (outcome_cell(item)) }
                        }
                    }
                }
            }
            @if plan.is_empty() {
                p class="muted" { "This collection has no pieces." }
            }
            @if assign.is_empty() {
                p class="muted" { "Nothing to assign." }
            } @else {
                form method="post" action=(&action) {
                    (layout::csrf_field(&token))
                    // The player is carried by id, not username: usernames can
                    // be renamed, and the write must go to the person previewed.
                    input type="hidden" name="user_id" value=(player.id);
                    @if let Some(part) = raw_part {
                        input type="hidden" name="part" value=(part);
                    }
                    @for item in &assign {
                        @if let Some(v) = &item.voice {
                            input type="hidden" name="pair"
                                  value=(format!("{}:{}", item.collection_item_id, v.voice_id));
                        }
                    }
                    button type="submit" {
                        "Assign " (player.username) " to " (assign.len()) " "
                        @if assign.len() == 1 { "piece" } @else { "pieces" }
                    }
                }
            }
        }

        p {
            a href=(format!("/admin/orgs/{org_id}/collections/{collection_id}")) {
                "← Back to the collection"
            }
            " · "
            a href=(format!("/admin/orgs/{org_id}/coverage/{collection_id}")) { "Coverage" }
        }
    };
    let mut response =
        Html(console::console_page(&ctx, Section::Collections, body).into_string()).into_response();
    *response.status_mut() = status;
    response
}

fn parse_pair(raw: &str) -> Option<(Uuid, Uuid)> {
    let (item, voice) = raw.split_once(':')?;
    Some((Uuid::parse_str(item).ok()?, Uuid::parse_str(voice).ok()?))
}

async fn apply_bulk_assign(
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

    let Some(player_id) = field(&pairs, "user_id").and_then(|v| Uuid::parse_str(v).ok()) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Choose a player and preview first.",
        );
    };
    let Some(raw_part) = field(&pairs, "part") else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Choose a part and preview first.",
        );
    };
    let Some(key) = parse_part(raw_part) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Choose a part.");
    };
    let raw_pairs = multi(&pairs, "pair");
    let Some(assignments) = raw_pairs
        .iter()
        .map(|p| parse_pair(p))
        .collect::<Option<Vec<_>>>()
    else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Malformed assignment list.");
    };

    match bulk_assign::apply(
        &state.db,
        collection_id,
        key,
        player_id,
        &assignments,
        &audit_ctx(&ctx, request_id),
    )
    .await
    {
        Ok(_) => {}
        Err(bulk_assign::BulkAssignError::Empty) => {
            return error_page(
                &ctx,
                StatusCode::BAD_REQUEST,
                "There was nothing to assign.",
            )
        }
        Err(bulk_assign::BulkAssignError::UnknownUser) => {
            return error_page(&ctx, StatusCode::NOT_FOUND, "That user no longer exists.")
        }
        Err(bulk_assign::BulkAssignError::Stale) => {
            return error_page(
                &ctx,
                StatusCode::CONFLICT,
                "The collection changed since the preview — a piece or voice was removed, \
                 or a voice is no longer this part. Nothing was assigned; go back and \
                 preview again.",
            )
        }
        Err(bulk_assign::BulkAssignError::Database(error)) => {
            tracing::error!(%error, "failed to apply a bulk assignment");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not assign the parts. Nothing was assigned.",
            );
        }
    }

    // The redirect shows the preview again under the player's CURRENT
    // username; the id is what was written to.
    let username = match user::find_by_id(&state.db, player_id).await {
        Ok(Some(player)) => player.username,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "That user no longer exists."),
        Err(error) => {
            tracing::error!(%error, "failed to look up the player after assigning");
            return Redirect::to(&page_url(org_id, collection_id)).into_response();
        }
    };

    // Back to the same preview: what was just assigned now reads "already
    // plays it", which is the confirmation.
    let back = format!(
        "{}?username={}&part={}",
        page_url(org_id, collection_id),
        encode_query_value(&username),
        encode_query_value(&encode_part(key)),
    );
    Redirect::to(&back).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_values_round_trip() {
        let instrument_id = Uuid::now_v7();
        for part_number in [Some(1), Some(12), None] {
            let key = PartKey {
                instrument_id,
                part_number,
            };
            assert_eq!(parse_part(&encode_part(key)), Some(key));
        }
    }

    #[test]
    fn malformed_part_values_are_rejected() {
        let id = Uuid::now_v7();
        assert_eq!(parse_part("nonsense"), None);
        assert_eq!(parse_part(&format!("{id}")), None);
        assert_eq!(parse_part(&format!("{id}:0")), None);
        assert_eq!(parse_part(&format!("{id}:x")), None);
        assert_eq!(parse_part("not-a-uuid:1"), None);
    }

    #[test]
    fn pairs_parse_or_fail_whole() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(parse_pair(&format!("{a}:{b}")), Some((a, b)));
        assert_eq!(parse_pair(&format!("{a}")), None);
        assert_eq!(parse_pair(&format!("{a}:junk")), None);
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode_query_value("alice"), "alice");
        assert_eq!(encode_query_value("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(encode_query_value("jörg"), "j%C3%B6rg");
        assert_eq!(encode_query_value("x:1"), "x%3A1");
    }
}
