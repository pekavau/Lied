//! Bulk assignment: one player on "the same part" in every piece of a
//! collection — "the 1st trumpet on the whole March Book" (issue #55).
//!
//! **It fans out into ordinary assignments; nothing new is stored.** Each
//! result is a normal [`crate::domain::part_assignment`] row, individually
//! editable afterwards, so moving one piece to a different player is the same
//! edit it always was, and there is no standing roster to drift out of sync
//! with the assignments themselves. A piece added to the collection later is
//! picked up by running bulk assignment again — pieces the player already
//! holds are reported, not duplicated.
//!
//! **Two steps, because the match is only as good as the data.** [`plan`] is a
//! read: for every piece it says what *would* happen — assign this voice,
//! already held, no such part here, more than one candidate, piece removed.
//! [`apply`] then writes exactly the `(item, voice)` pairs the caller accepted
//! from that plan, not a fresh re-match, so what is written is what was shown.
//! If the collection changed in between (a piece or voice removed), the whole
//! write is refused with [`BulkAssignError::Stale`] and nothing is assigned.
//!
//! **What "the same part" means.** A voice matches when it has the requested
//! `instrument_id` and the same `part_number` — including both being `NULL`,
//! which is how unnumbered parts (Timpani, Harp) are matched. Two matching
//! voices in one piece is ambiguous and is reported rather than guessed at:
//! one person cannot play both.
//!
//! **Conflicts are reported, never blocking** (the coverage rules): if the
//! player already holds a different voice of a piece, the plan says so next to
//! the assignment it would add, and [`apply`] still adds it.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::part_assignment::PartAssignment;

/// "The same part across pieces": an instrument and its part number, where a
/// `None` part number is matched to voices that carry none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartKey {
    pub instrument_id: Uuid,
    pub part_number: Option<i16>,
}

/// A voice as the plan reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VoiceRef {
    pub voice_id: Uuid,
    pub name: String,
    pub part_number: Option<i16>,
}

/// What bulk assignment would do with one piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// Exactly one voice matches and the player does not hold it yet.
    Assign,
    /// Exactly one voice matches and the player already plays it.
    AlreadyHeld,
    /// No voice of this piece matches. `candidates` lists the piece's voices
    /// for the same instrument, so a part named differently (or missing its
    /// part number) is visible rather than silently skipped.
    NoMatch,
    /// More than one voice matches; `candidates` lists them. Assign by hand.
    Ambiguous,
    /// The piece's arrangement was removed from the catalog; it takes no new
    /// assignees until restored.
    Removed,
}

/// The plan for one piece of the collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub collection_item_id: Uuid,
    pub index: i32,
    pub arrangement_title: String,
    pub outcome: Outcome,
    /// The matched voice, for [`Outcome::Assign`] and [`Outcome::AlreadyHeld`].
    pub voice: Option<VoiceRef>,
    /// For [`Outcome::Ambiguous`]: the matching voices. For
    /// [`Outcome::NoMatch`]: every voice of the same instrument in the piece.
    pub candidates: Vec<VoiceRef>,
    /// For [`Outcome::Assign`]: other voices of this piece the player already
    /// holds. Not an error — sections get juggled — but one person cannot play
    /// two parts at once, so it is surfaced next to the assignment.
    pub also_holds: Vec<VoiceRef>,
}

#[derive(thiserror::Error, Debug)]
pub enum BulkAssignError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// No pairs were submitted.
    #[error("nothing to assign")]
    Empty,
    /// The player does not exist.
    #[error("unknown user")]
    UnknownUser,
    /// At least one submitted pair no longer names a live piece of this
    /// collection and a live voice of that piece's arrangement. Nothing was
    /// written.
    #[error("the collection changed since the plan was made")]
    Stale,
}

/// What [`apply`] did.
#[derive(Debug, Clone)]
pub struct Applied {
    /// The assignments created, in the order the pairs were submitted.
    pub created: Vec<PartAssignment>,
    /// Submitted `(collection_item_id, voice_id)` pairs the player already
    /// held — left as they were.
    pub already_held: Vec<(Uuid, Uuid)>,
}

/// One piece × one voice of the requested instrument (or a piece with none).
#[derive(Debug, Clone)]
struct CandidateRow {
    item_id: Uuid,
    index: i32,
    arrangement_title: String,
    arrangement_removed: bool,
    voice: Option<VoiceRef>,
    held: bool,
}

/// What would assigning `user_id` to `key` across `collection_id` do?
///
/// Every live piece of the collection appears exactly once, in running order.
/// A soft-deleted collection yields an empty plan; callers resolve the
/// collection first so that case is a 404 before it gets here.
pub async fn plan(
    pool: &PgPool,
    collection_id: Uuid,
    key: PartKey,
    user_id: Uuid,
) -> Result<Vec<PlanItem>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id as item_id,
            ci.index as item_index,
            a.title as arrangement_title,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!",
            v.id as "voice_id?",
            v.name as "voice_name?",
            v.part_number as "part_number?",
            EXISTS (
                SELECT 1 FROM part_assignment pa
                WHERE pa.collection_item_id = ci.id
                  AND pa.voice_id = v.id
                  AND pa.user_id = $3
            ) as "held!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        -- LEFT JOIN: a piece without this instrument must still appear, as a
        -- reported skip, not vanish from the plan.
        LEFT JOIN voice v
          ON v.arrangement_id = a.id
         AND v.deleted_at IS NULL
         AND v.instrument_id = $2
        WHERE ci.collection_id = $1
          AND ci.deleted_at IS NULL
          AND c.deleted_at IS NULL
        ORDER BY ci.index ASC, ci.id ASC, v.part_number ASC NULLS LAST, v.name ASC, v.id ASC
        "#,
        collection_id,
        key.instrument_id,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    // Every live voice the player already holds in this collection, any
    // instrument — the conflict note on an `Assign`.
    let holdings = sqlx::query!(
        r#"
        SELECT pa.collection_item_id, v.id as voice_id, v.name, v.part_number
        FROM part_assignment pa
        JOIN collection_item ci ON ci.id = pa.collection_item_id
        JOIN voice v ON v.id = pa.voice_id
        WHERE ci.collection_id = $1
          AND ci.deleted_at IS NULL
          AND v.deleted_at IS NULL
          AND pa.user_id = $2
        ORDER BY v.name ASC, v.id ASC
        "#,
        collection_id,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    let mut held_by_item: HashMap<Uuid, Vec<VoiceRef>> = HashMap::new();
    for h in holdings {
        held_by_item
            .entry(h.collection_item_id)
            .or_default()
            .push(VoiceRef {
                voice_id: h.voice_id,
                name: h.name,
                part_number: h.part_number,
            });
    }

    let candidates = rows
        .into_iter()
        .map(|r| CandidateRow {
            item_id: r.item_id,
            index: r.item_index,
            arrangement_title: r.arrangement_title,
            arrangement_removed: r.arrangement_removed,
            voice: match (r.voice_id, r.voice_name) {
                (Some(voice_id), Some(name)) => Some(VoiceRef {
                    voice_id,
                    name,
                    part_number: r.part_number,
                }),
                _ => None,
            },
            held: r.held,
        })
        .collect::<Vec<_>>();

    Ok(assemble(candidates, key.part_number, &held_by_item))
}

/// Turn the per-(piece, voice) rows into one decision per piece. Pure, so the
/// matching rules are unit-tested without a database.
fn assemble(
    rows: Vec<CandidateRow>,
    part_number: Option<i16>,
    held_by_item: &HashMap<Uuid, Vec<VoiceRef>>,
) -> Vec<PlanItem> {
    // Group consecutive rows by piece (the query orders by piece first).
    let mut groups: Vec<Vec<CandidateRow>> = Vec::new();
    for row in rows {
        match groups.last_mut() {
            Some(group) if group[0].item_id == row.item_id => group.push(row),
            _ => groups.push(vec![row]),
        }
    }

    groups
        .into_iter()
        .map(|group| {
            let first = &group[0];
            let mut item = PlanItem {
                collection_item_id: first.item_id,
                index: first.index,
                arrangement_title: first.arrangement_title.clone(),
                outcome: Outcome::NoMatch,
                voice: None,
                candidates: Vec::new(),
                also_holds: Vec::new(),
            };
            if first.arrangement_removed {
                item.outcome = Outcome::Removed;
                return item;
            }

            let instrument_voices: Vec<(VoiceRef, bool)> = group
                .into_iter()
                .filter_map(|r| r.voice.map(|v| (v, r.held)))
                .collect();
            let mut matching: Vec<(VoiceRef, bool)> = instrument_voices
                .iter()
                .filter(|(v, _)| v.part_number == part_number)
                .cloned()
                .collect();

            match matching.len() {
                0 => {
                    item.outcome = Outcome::NoMatch;
                    item.candidates = instrument_voices.into_iter().map(|(v, _)| v).collect();
                }
                1 => {
                    let (voice, held) = matching.remove(0);
                    if held {
                        item.outcome = Outcome::AlreadyHeld;
                    } else {
                        item.outcome = Outcome::Assign;
                        item.also_holds = held_by_item
                            .get(&item.collection_item_id)
                            .map(|vs| {
                                vs.iter()
                                    .filter(|v| v.voice_id != voice.voice_id)
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                    item.voice = Some(voice);
                }
                _ => {
                    item.outcome = Outcome::Ambiguous;
                    item.candidates = matching.into_iter().map(|(v, _)| v).collect();
                }
            }
            item
        })
        .collect()
}

/// Assign `user_id` to each submitted `(collection_item_id, voice_id)` pair of
/// `collection_id`, all or nothing.
///
/// Every pair must name a live piece of this collection whose arrangement is
/// live, and a live voice of that arrangement; one that does not means the
/// collection changed since the plan was made, and the call fails with
/// [`BulkAssignError::Stale`] having written nothing. The rows it checked are
/// share-locked for the rest of the transaction, so they cannot be removed
/// between the check and the insert.
///
/// Pairs the player already holds are left alone and reported in
/// [`Applied::already_held`] — re-running bulk assignment after adding a piece
/// is the intended way to extend it. Duplicate pairs in the input are ignored.
pub async fn apply(
    pool: &PgPool,
    collection_id: Uuid,
    user_id: Uuid,
    pairs: &[(Uuid, Uuid)],
    created_by: Option<Uuid>,
) -> Result<Applied, BulkAssignError> {
    let mut seen = HashSet::new();
    let pairs: Vec<(Uuid, Uuid)> = pairs.iter().copied().filter(|p| seen.insert(*p)).collect();
    if pairs.is_empty() {
        return Err(BulkAssignError::Empty);
    }
    let item_ids: Vec<Uuid> = pairs.iter().map(|(item, _)| *item).collect();
    let voice_ids: Vec<Uuid> = pairs.iter().map(|(_, voice)| *voice).collect();
    let ids: Vec<Uuid> = pairs.iter().map(|_| Uuid::now_v7()).collect();

    let mut tx = pool.begin().await?;

    let user_exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM "user" WHERE id = $1) as "exists!""#,
        user_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if !user_exists {
        return Err(BulkAssignError::UnknownUser);
    }

    let valid = sqlx::query_scalar!(
        r#"
        SELECT ci.id
        FROM unnest($2::uuid[], $3::uuid[]) AS p(item_id, voice_id)
        JOIN collection_item ci
          ON ci.id = p.item_id
         AND ci.collection_id = $1
         AND ci.deleted_at IS NULL
        JOIN collection c ON c.id = ci.collection_id AND c.deleted_at IS NULL
        JOIN arrangement a ON a.id = ci.arrangement_id AND a.deleted_at IS NULL
        JOIN voice v
          ON v.id = p.voice_id
         AND v.arrangement_id = a.id
         AND v.deleted_at IS NULL
        FOR SHARE OF ci, c, a, v
        "#,
        collection_id,
        &item_ids,
        &voice_ids,
    )
    .fetch_all(&mut *tx)
    .await?;
    if valid.len() != pairs.len() {
        return Err(BulkAssignError::Stale);
    }

    let inserted = sqlx::query_as!(
        PartAssignment,
        r#"
        INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id, created_by)
        SELECT p.id, p.item_id, $1, p.voice_id, $2
        FROM unnest($3::uuid[], $4::uuid[], $5::uuid[]) WITH ORDINALITY
             AS p(id, item_id, voice_id, ord)
        ORDER BY p.ord
        ON CONFLICT (collection_item_id, voice_id, user_id) DO NOTHING
        RETURNING
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        "#,
        user_id,
        created_by,
        &ids,
        &item_ids,
        &voice_ids,
    )
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    // RETURNING carries no ordering guarantee; restore submission order.
    let mut by_pair: HashMap<(Uuid, Uuid), PartAssignment> = inserted
        .into_iter()
        .map(|a| ((a.collection_item_id, a.voice_id), a))
        .collect();
    let mut created = Vec::new();
    let mut already_held = Vec::new();
    for pair in pairs {
        match by_pair.remove(&pair) {
            Some(assignment) => created.push(assignment),
            None => already_held.push(pair),
        }
    }
    Ok(Applied {
        created,
        already_held,
    })
}

/// A part present somewhere in a collection: what the console offers to pick
/// from, so the archivist chooses among parts that exist rather than typing a
/// part number blind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInCollection {
    pub instrument_id: Uuid,
    pub instrument_name: String,
    pub part_number: Option<i16>,
    /// How many live pieces of the collection have this part.
    pub pieces: i64,
}

/// Every `(instrument, part number)` among the live voices of the live pieces
/// of a collection, with how many pieces carry it.
pub async fn parts_in_collection(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<Vec<PartInCollection>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            i.id as instrument_id,
            i.display_name as instrument_name,
            v.part_number as "part_number?",
            count(DISTINCT ci.id) as "pieces!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id AND c.deleted_at IS NULL
        JOIN arrangement a ON a.id = ci.arrangement_id AND a.deleted_at IS NULL
        JOIN voice v ON v.arrangement_id = a.id AND v.deleted_at IS NULL
        JOIN instrument i ON i.id = v.instrument_id
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL
        GROUP BY i.id, i.display_name, v.part_number
        ORDER BY i.display_name ASC, v.part_number ASC NULLS LAST
        "#,
        collection_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PartInCollection {
            instrument_id: r.instrument_id,
            instrument_name: r.instrument_name,
            part_number: r.part_number,
            pieces: r.pieces,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(name: &str, part_number: Option<i16>) -> VoiceRef {
        VoiceRef {
            voice_id: Uuid::now_v7(),
            name: name.to_string(),
            part_number,
        }
    }

    fn row(item: Uuid, index: i32, voice: Option<VoiceRef>, held: bool) -> CandidateRow {
        CandidateRow {
            item_id: item,
            index,
            arrangement_title: format!("Piece {index}"),
            arrangement_removed: false,
            voice,
            held,
        }
    }

    #[test]
    fn one_matching_voice_is_assigned() {
        let item = Uuid::now_v7();
        let first = voice("Trumpet in Bb 1", Some(1));
        let plan = assemble(
            vec![
                row(item, 1, Some(first.clone()), false),
                row(item, 1, Some(voice("Trumpet in Bb 2", Some(2))), false),
            ],
            Some(1),
            &HashMap::new(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].outcome, Outcome::Assign);
        assert_eq!(plan[0].voice, Some(first));
        assert!(plan[0].candidates.is_empty());
    }

    #[test]
    fn a_held_voice_is_reported_not_reassigned() {
        let item = Uuid::now_v7();
        let plan = assemble(
            vec![row(item, 1, Some(voice("Trumpet 1", Some(1))), true)],
            Some(1),
            &HashMap::new(),
        );
        assert_eq!(plan[0].outcome, Outcome::AlreadyHeld);
    }

    #[test]
    fn a_piece_without_the_part_lists_the_instruments_other_voices() {
        let item = Uuid::now_v7();
        let unnumbered = voice("Trumpet", None);
        let second = voice("Trumpet 2", Some(2));
        let plan = assemble(
            vec![
                row(item, 1, Some(second.clone()), false),
                row(item, 1, Some(unnumbered.clone()), false),
            ],
            Some(1),
            &HashMap::new(),
        );
        assert_eq!(plan[0].outcome, Outcome::NoMatch);
        assert_eq!(plan[0].candidates, vec![second, unnumbered]);
        assert_eq!(plan[0].voice, None);
    }

    #[test]
    fn a_piece_without_the_instrument_is_a_no_match_with_no_candidates() {
        let item = Uuid::now_v7();
        let plan = assemble(vec![row(item, 1, None, false)], Some(1), &HashMap::new());
        assert_eq!(plan[0].outcome, Outcome::NoMatch);
        assert!(plan[0].candidates.is_empty());
    }

    #[test]
    fn unnumbered_parts_match_unnumbered_voices() {
        let item = Uuid::now_v7();
        let timpani = voice("Timpani", None);
        let plan = assemble(
            vec![row(item, 1, Some(timpani.clone()), false)],
            None,
            &HashMap::new(),
        );
        assert_eq!(plan[0].outcome, Outcome::Assign);
        assert_eq!(plan[0].voice, Some(timpani));

        // ...and a numbered request does not match them.
        let plan = assemble(
            vec![row(item, 1, Some(voice("Timpani", None)), false)],
            Some(1),
            &HashMap::new(),
        );
        assert_eq!(plan[0].outcome, Outcome::NoMatch);
    }

    #[test]
    fn two_matching_voices_are_ambiguous() {
        let item = Uuid::now_v7();
        let a = voice("Trumpet 1", Some(1));
        let b = voice("Trumpet 1 (alt)", Some(1));
        let plan = assemble(
            vec![
                row(item, 1, Some(a.clone()), false),
                row(item, 1, Some(b.clone()), false),
            ],
            Some(1),
            &HashMap::new(),
        );
        assert_eq!(plan[0].outcome, Outcome::Ambiguous);
        assert_eq!(plan[0].candidates, vec![a, b]);
        assert_eq!(plan[0].voice, None);
    }

    #[test]
    fn a_removed_piece_is_skipped_whatever_its_voices() {
        let item = Uuid::now_v7();
        let mut r = row(item, 1, Some(voice("Trumpet 1", Some(1))), false);
        r.arrangement_removed = true;
        let plan = assemble(vec![r], Some(1), &HashMap::new());
        assert_eq!(plan[0].outcome, Outcome::Removed);
        assert_eq!(plan[0].voice, None);
    }

    #[test]
    fn other_parts_the_player_holds_are_reported_as_a_conflict() {
        let item = Uuid::now_v7();
        let target = voice("Trumpet 1", Some(1));
        let other = voice("Flugelhorn", None);
        let mut held = HashMap::new();
        held.insert(item, vec![other.clone()]);
        let plan = assemble(vec![row(item, 1, Some(target), false)], Some(1), &held);
        assert_eq!(plan[0].outcome, Outcome::Assign);
        assert_eq!(plan[0].also_holds, vec![other]);
    }

    #[test]
    fn every_piece_appears_once_in_order() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let plan = assemble(
            vec![
                row(a, 1, Some(voice("Trumpet 1", Some(1))), false),
                row(b, 2, None, false),
                row(c, 3, Some(voice("Trumpet 1", Some(1))), false),
                row(c, 3, Some(voice("Trumpet 2", Some(2))), false),
            ],
            Some(1),
            &HashMap::new(),
        );
        let indexes: Vec<i32> = plan.iter().map(|p| p.index).collect();
        assert_eq!(indexes, vec![1, 2, 3]);
        let outcomes: Vec<Outcome> = plan.iter().map(|p| p.outcome).collect();
        assert_eq!(
            outcomes,
            vec![Outcome::Assign, Outcome::NoMatch, Outcome::Assign]
        );
    }
}
