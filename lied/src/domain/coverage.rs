//! Coverage: is every required voice in a collection covered, and rehearsed?
//! (UC-13, issue #35.)
//!
//! Pure query over existing tables — nothing here is stored. The state of a
//! voice is read from whether a [`crate::domain::part_assignment`] row exists
//! for it and how far that assignment has got:
//!
//!   * **unassigned** — nobody is down to play it;
//!   * **assigned** — someone is, but they have not acknowledged;
//!   * **rehearsed** — `acknowledged_at` is set, which is CLAUDE.md's
//!     definition of rehearsed coverage.
//!
//! `notified_at` is carried alongside rather than folded into those states,
//! because "assigned but nobody has told them" and "told, but no reply" are
//! different problems with different fixes, and chasing the second is the
//! archivist's week before a concert.
//!
//! **Two views, one query.** [`Required`] is the only thing that differs
//! between the staff view of a whole programme and a principal's view of their
//! own section — everything downstream (grouping, counting, rendering) is
//! shared, so the two cannot drift into disagreeing about what "covered" means.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Which voices a report counts as required.
#[derive(Debug, Clone)]
pub enum Required {
    /// Every live voice of every live piece — the staff view of a programme.
    EveryVoice,
    /// Only voices for these instruments — a principal's section. An empty
    /// list is honest rather than an error: it means the principal has no
    /// instruments configured, which is a true (and fixable) answer.
    Instruments(Vec<Uuid>),
}

/// How far one voice has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum VoiceState {
    Unassigned,
    Assigned,
    Rehearsed,
}

/// One voice of one piece, with who is playing it and how far along they are.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCoverage {
    pub voice_id: Uuid,
    pub voice_name: String,
    pub instrument_id: Uuid,
    pub state: VoiceState,
    pub assignee_username: Option<String>,
    pub assignee_display_name: Option<String>,
    pub notified_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

/// One piece of the collection, with its voices and their tallies.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemCoverage {
    pub item_id: Uuid,
    pub index: i32,
    pub arrangement_id: Uuid,
    pub arrangement_title: String,
    pub arrangement_slug: String,
    /// The arrangement was removed from the catalogue. The piece still holds
    /// its slot in the programme (hide-with-references), but its voices are
    /// **not** counted as required — a slot that has lost its music is a
    /// problem to see, not a gap to chase.
    pub arrangement_removed: bool,
    pub voices: Vec<VoiceCoverage>,
    pub required: usize,
    pub assigned: usize,
    pub rehearsed: usize,
}

/// A whole collection's coverage.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    pub collection_id: Uuid,
    pub items: Vec<ItemCoverage>,
    pub required: usize,
    pub assigned: usize,
    pub rehearsed: usize,
}

impl CoverageReport {
    /// Whether every required voice is assigned. `true` for an empty report —
    /// a programme with nothing required has no gaps, which is what the
    /// dashboard should say about an empty collection.
    pub fn is_fully_assigned(&self) -> bool {
        self.assigned == self.required
    }
}

/// Coverage for one collection.
///
/// Soft-deleted items, voices and arrangements never count as required. A
/// removed arrangement's item is still returned (flagged, with no voices) so
/// the caller can show the broken slot; every other exclusion is silent
/// because there is nothing to show.
pub async fn for_collection(
    pool: &PgPool,
    collection_id: Uuid,
    required: &Required,
) -> Result<CoverageReport, sqlx::Error> {
    // `Instruments([])` and `EveryVoice` are different questions: the first
    // requires nothing, the second requires everything. Passing NULL for the
    // filter means "no filter", so the empty-section case is expressed as an
    // empty array rather than NULL.
    let instrument_filter: Option<Vec<Uuid>> = match required {
        Required::EveryVoice => None,
        Required::Instruments(ids) => Some(ids.clone()),
    };

    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id as item_id,
            ci.index as item_index,
            a.id as arrangement_id,
            a.title as arrangement_title,
            a.slug as arrangement_slug,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!",
            v.id as "voice_id?",
            v.name as "voice_name?",
            v.instrument_id as "instrument_id?",
            u.username as "assignee_username?",
            u.display_name as "assignee_display_name?",
            pa.id as "assignment_id?",
            pa.notified_at as "notified_at?: DateTime<Utc>",
            pa.acknowledged_at as "acknowledged_at?: DateTime<Utc>"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        -- LEFT JOIN, not JOIN: a piece with no live voices (or whose
        -- arrangement was removed) must still appear as a row, or the
        -- programme would silently lose the slot that most needs attention.
        LEFT JOIN voice v
          ON v.arrangement_id = a.id
         AND v.deleted_at IS NULL
         AND a.deleted_at IS NULL
         AND ($2::uuid[] IS NULL OR v.instrument_id = ANY($2))
        LEFT JOIN part_assignment pa
          ON pa.collection_item_id = ci.id
         AND pa.voice_id = v.id
        LEFT JOIN "user" u ON u.id = pa.user_id
        WHERE ci.collection_id = $1
          AND ci.deleted_at IS NULL
          AND c.deleted_at IS NULL
        ORDER BY ci.index ASC, ci.id ASC, v.name ASC, v.id ASC
        "#,
        collection_id,
        instrument_filter.as_deref(),
    )
    .fetch_all(pool)
    .await?;

    let mut items: Vec<ItemCoverage> = Vec::new();
    for row in rows {
        if items.last().map(|item| item.item_id) != Some(row.item_id) {
            items.push(ItemCoverage {
                item_id: row.item_id,
                index: row.item_index,
                arrangement_id: row.arrangement_id,
                arrangement_title: row.arrangement_title,
                arrangement_slug: row.arrangement_slug,
                arrangement_removed: row.arrangement_removed,
                voices: Vec::new(),
                required: 0,
                assigned: 0,
                rehearsed: 0,
            });
        }
        let item = items.last_mut().expect("just pushed");

        // The LEFT JOIN yields one all-NULL voice row for a piece with nothing
        // required; that is the marker for "no voices", not a voice.
        let (Some(voice_id), Some(voice_name), Some(instrument_id)) =
            (row.voice_id, row.voice_name, row.instrument_id)
        else {
            continue;
        };

        let state = classify(row.assignment_id, row.acknowledged_at);
        item.required += 1;
        match state {
            VoiceState::Unassigned => {}
            // Rehearsed implies assigned: a voice that is done is not also a
            // gap, and the two counts are read as "of N required, A are
            // covered and R of those are ready".
            VoiceState::Assigned => item.assigned += 1,
            VoiceState::Rehearsed => {
                item.assigned += 1;
                item.rehearsed += 1;
            }
        }
        item.voices.push(VoiceCoverage {
            voice_id,
            voice_name,
            instrument_id,
            state,
            assignee_username: row.assignee_username,
            assignee_display_name: row.assignee_display_name,
            notified_at: row.notified_at,
            acknowledged_at: row.acknowledged_at,
        });
    }

    let required_total = items.iter().map(|i| i.required).sum();
    let assigned_total = items.iter().map(|i| i.assigned).sum();
    let rehearsed_total = items.iter().map(|i| i.rehearsed).sum();

    Ok(CoverageReport {
        collection_id,
        items,
        required: required_total,
        assigned: assigned_total,
        rehearsed: rehearsed_total,
    })
}

/// Coverage for **every** collection in an org, in one query.
///
/// The dashboard renders one row per collection, and looping
/// [`for_collection`] over them would issue a five-way join per row — the N+1
/// that #33's review caught in the assignee list, with a heavier query behind
/// it. The grouping below is the same as [`for_collection`]'s with one more
/// level on top.
///
/// Collections with no live items still appear (with zero tallies): "this
/// programme is empty" is something the dashboard must be able to say.
pub async fn for_org(
    pool: &PgPool,
    organization_id: Uuid,
    required: &Required,
) -> Result<Vec<CoverageReport>, sqlx::Error> {
    let instrument_filter: Option<Vec<Uuid>> = match required {
        Required::EveryVoice => None,
        Required::Instruments(ids) => Some(ids.clone()),
    };

    let rows = sqlx::query!(
        r#"
        SELECT
            c.id as collection_id,
            ci.id as "item_id?",
            ci.index as "item_index?",
            a.id as "arrangement_id?",
            a.title as "arrangement_title?",
            a.slug as "arrangement_slug?",
            (a.deleted_at IS NOT NULL) as "arrangement_removed?",
            v.id as "voice_id?",
            v.name as "voice_name?",
            v.instrument_id as "instrument_id?",
            u.username as "assignee_username?",
            u.display_name as "assignee_display_name?",
            pa.id as "assignment_id?",
            pa.notified_at as "notified_at?: DateTime<Utc>",
            pa.acknowledged_at as "acknowledged_at?: DateTime<Utc>"
        FROM collection c
        LEFT JOIN collection_item ci
          ON ci.collection_id = c.id AND ci.deleted_at IS NULL
        LEFT JOIN arrangement a ON a.id = ci.arrangement_id
        LEFT JOIN voice v
          ON v.arrangement_id = a.id
         AND v.deleted_at IS NULL
         AND a.deleted_at IS NULL
         AND ($2::uuid[] IS NULL OR v.instrument_id = ANY($2))
        LEFT JOIN part_assignment pa
          ON pa.collection_item_id = ci.id AND pa.voice_id = v.id
        LEFT JOIN "user" u ON u.id = pa.user_id
        WHERE c.organization_id = $1 AND c.deleted_at IS NULL
        ORDER BY c.name ASC, c.id ASC, ci.index ASC, ci.id ASC, v.name ASC, v.id ASC
        "#,
        organization_id,
        instrument_filter.as_deref(),
    )
    .fetch_all(pool)
    .await?;

    let mut reports: Vec<CoverageReport> = Vec::new();
    for row in rows {
        if reports.last().map(|r| r.collection_id) != Some(row.collection_id) {
            reports.push(CoverageReport {
                collection_id: row.collection_id,
                items: Vec::new(),
                required: 0,
                assigned: 0,
                rehearsed: 0,
            });
        }
        let report = reports.last_mut().expect("just pushed");

        // A collection with no live items yields one all-NULL row.
        let (Some(item_id), Some(index), Some(arrangement_id)) =
            (row.item_id, row.item_index, row.arrangement_id)
        else {
            continue;
        };
        if report.items.last().map(|i| i.item_id) != Some(item_id) {
            report.items.push(ItemCoverage {
                item_id,
                index,
                arrangement_id,
                arrangement_title: row.arrangement_title.unwrap_or_default(),
                arrangement_slug: row.arrangement_slug.unwrap_or_default(),
                arrangement_removed: row.arrangement_removed.unwrap_or(false),
                voices: Vec::new(),
                required: 0,
                assigned: 0,
                rehearsed: 0,
            });
        }
        let item = report.items.last_mut().expect("just pushed");

        let (Some(voice_id), Some(voice_name), Some(instrument_id)) =
            (row.voice_id, row.voice_name, row.instrument_id)
        else {
            continue;
        };
        let state = classify(row.assignment_id, row.acknowledged_at);
        item.required += 1;
        match state {
            VoiceState::Unassigned => {}
            VoiceState::Assigned => item.assigned += 1,
            VoiceState::Rehearsed => {
                item.assigned += 1;
                item.rehearsed += 1;
            }
        }
        item.voices.push(VoiceCoverage {
            voice_id,
            voice_name,
            instrument_id,
            state,
            assignee_username: row.assignee_username,
            assignee_display_name: row.assignee_display_name,
            notified_at: row.notified_at,
            acknowledged_at: row.acknowledged_at,
        });
    }

    for report in &mut reports {
        report.required = report.items.iter().map(|i| i.required).sum();
        report.assigned = report.items.iter().map(|i| i.assigned).sum();
        report.rehearsed = report.items.iter().map(|i| i.rehearsed).sum();
    }
    Ok(reports)
}

/// The one place a `(assignment, acknowledged_at)` pair becomes a state, shared
/// by both queries so they cannot classify differently.
fn classify(assignment_id: Option<Uuid>, acknowledged_at: Option<DateTime<Utc>>) -> VoiceState {
    match (assignment_id, acknowledged_at) {
        (None, _) => VoiceState::Unassigned,
        (Some(_), None) => VoiceState::Assigned,
        (Some(_), Some(_)) => VoiceState::Rehearsed,
    }
}
