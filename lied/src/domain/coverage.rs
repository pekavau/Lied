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

/// One musician playing one part, and how far their assignment has got.
///
/// `notified_at` stays separate from `acknowledged_at` because "assigned but
/// nobody has told them" and "told, but no reply" are different problems with
/// different fixes.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub assignment_id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub display_name: String,
    pub notified_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

/// One voice of one piece and everyone playing it.
///
/// Acknowledgement is deliberately **not** reduced to a per-voice flag: a
/// section where two of three players have replied is neither "rehearsed" nor
/// "not rehearsed", and collapsing it would hide which. The ratio is rendered
/// instead (issue #53).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCoverage {
    pub voice_id: Uuid,
    pub voice_name: String,
    pub instrument_id: Uuid,
    pub players: Vec<Player>,
}

impl VoiceCoverage {
    /// A part is covered once anybody is playing it. Whether *enough* people
    /// are is a question the model has no desk counts to answer (#55).
    pub fn is_covered(&self) -> bool {
        !self.players.is_empty()
    }

    pub fn acknowledged(&self) -> usize {
        self.players
            .iter()
            .filter(|p| p.acknowledged_at.is_some())
            .count()
    }
}

/// One musician holding several voices in the same piece.
///
/// Legal and sometimes necessary — sections get juggled by who turns up — but
/// nobody plays two parts at once, so a piece covered this way is not as ready
/// as its tallies suggest. Reported, never prevented.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub user_id: Uuid,
    pub username: String,
    pub display_name: String,
    pub voice_names: Vec<String>,
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
    /// Parts: how many voices this piece needs, and how many have anybody on
    /// them.
    pub required: usize,
    pub covered: usize,
    /// Players: how many **distinct people** are assigned across those parts,
    /// and how many of them have acknowledged everything they hold. A different
    /// question with a different denominator from the part counts, so the two
    /// must be labelled wherever they are shown.
    ///
    /// Distinct people, not assignment rows: somebody holding two voices of
    /// this piece is one musician who cannot be in two places — which is what
    /// `conflicts` says — and counting them twice would inflate a section that
    /// is in fact short-handed.
    pub players: usize,
    pub players_acknowledged: usize,
    /// Musicians holding more than one voice in *this* piece.
    pub conflicts: Vec<Conflict>,
}

/// A whole collection's coverage.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    pub collection_id: Uuid,
    pub items: Vec<ItemCoverage>,
    pub required: usize,
    pub covered: usize,
    pub players: usize,
    pub players_acknowledged: usize,
}

impl CoverageReport {
    /// Whether every required part has somebody on it. `true` for an empty
    /// report — a programme requiring nothing has no gaps, which is what the
    /// dashboard should say about an empty collection.
    pub fn is_fully_covered(&self) -> bool {
        self.covered == self.required
    }

    /// Every conflict in the collection, piece by piece.
    pub fn conflicts(&self) -> impl Iterator<Item = (&ItemCoverage, &Conflict)> {
        self.items
            .iter()
            .flat_map(|item| item.conflicts.iter().map(move |c| (item, c)))
    }
}

/// Fold one query row into the report being built.
///
/// The queries differ only in scope, so the grouping lives here once: piece →
/// voice → player, with each level starting a new group when its id changes.
/// The ORDER BY in both queries guarantees rows arrive grouped.
struct Grouper;

/// One query row's voice-and-player half, named so the ten nullable columns
/// cannot be passed in the wrong order — `username` and `display_name` are both
/// `Option<String>`, and swapping them would compile and be wrong forever.
pub(crate) struct VoiceRow {
    pub voice_id: Option<Uuid>,
    pub voice_name: Option<String>,
    pub instrument_id: Option<Uuid>,
    pub assignment_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub notified_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

impl Grouper {
    /// Add a voice row to `item`, starting a new voice when the id changes and
    /// appending a player when the row carries one.
    fn push_voice(item: &mut ItemCoverage, row: VoiceRow) {
        let VoiceRow {
            voice_id,
            voice_name,
            instrument_id,
            assignment_id,
            user_id,
            username,
            display_name,
            notified_at,
            acknowledged_at,
        } = row;
        // An all-NULL voice marks a piece with nothing required (no live
        // voices, or a removed arrangement) — not a voice.
        let (Some(voice_id), Some(voice_name), Some(instrument_id)) =
            (voice_id, voice_name, instrument_id)
        else {
            return;
        };
        if item.voices.last().map(|v| v.voice_id) != Some(voice_id) {
            item.voices.push(VoiceCoverage {
                voice_id,
                voice_name,
                instrument_id,
                players: Vec::new(),
            });
        }
        let voice = item.voices.last_mut().expect("just pushed");

        // A part with nobody on it yields one row with no assignment.
        let (Some(assignment_id), Some(user_id)) = (assignment_id, user_id) else {
            return;
        };
        voice.players.push(Player {
            assignment_id,
            user_id,
            username: username.unwrap_or_default(),
            display_name: display_name.unwrap_or_default(),
            notified_at,
            acknowledged_at,
        });
    }

    /// Roll a finished piece's voices up into its tallies and conflicts.
    ///
    /// A piece whose arrangement was removed from the catalogue **requires
    /// nothing** — its parts have no music behind them, so they are not gaps to
    /// chase. Its voices are still listed, because whoever was assigned to them
    /// needs to be visible to be cleaned up, and the assignment screen reads
    /// this same structure.
    fn finish_item(item: &mut ItemCoverage) {
        if item.arrangement_removed {
            // Requires nothing, and its players are not counted anywhere. The
            // voices themselves stay in `voices` so whoever holds them can be
            // seen and cleaned up — the JSON would otherwise claim zero players
            // beside a list of them.
            item.required = 0;
            item.covered = 0;
            item.players = 0;
            item.players_acknowledged = 0;
            item.conflicts = Vec::new();
            return;
        }
        item.required = item.voices.len();
        item.covered = item.voices.iter().filter(|v| v.is_covered()).count();

        // Distinct people. Somebody on two parts of this piece is one musician,
        // and counting them twice would make a short-handed section look fuller
        // than it is — the opposite of what this screen is for.
        let mut people: Vec<(Uuid, bool)> = Vec::new();
        for voice in &item.voices {
            for player in &voice.players {
                let acknowledged = player.acknowledged_at.is_some();
                match people.iter_mut().find(|(id, _)| *id == player.user_id) {
                    // Acknowledged everything they hold, or they are not done.
                    Some((_, all_acked)) => *all_acked = *all_acked && acknowledged,
                    None => people.push((player.user_id, acknowledged)),
                }
            }
        }
        item.players = people.len();
        item.players_acknowledged = people.iter().filter(|(_, acked)| *acked).count();

        // Somebody on more than one voice of this piece cannot play them at
        // once. Scoped to the piece: the same person on 1st trumpet in one
        // piece and 2nd in another is ordinary, since they are played at
        // different times.
        let mut seen: Vec<(Uuid, String, String, Vec<String>)> = Vec::new();
        for voice in &item.voices {
            for player in &voice.players {
                match seen.iter_mut().find(|(id, _, _, _)| *id == player.user_id) {
                    Some((_, _, _, voices)) => voices.push(voice.voice_name.clone()),
                    None => seen.push((
                        player.user_id,
                        player.username.clone(),
                        player.display_name.clone(),
                        vec![voice.voice_name.clone()],
                    )),
                }
            }
        }
        item.conflicts = seen
            .into_iter()
            .filter(|(_, _, _, voices)| voices.len() > 1)
            .map(|(user_id, username, display_name, voice_names)| Conflict {
                user_id,
                username,
                display_name,
                voice_names,
            })
            .collect();
    }

    /// Roll finished pieces up into a report's totals.
    ///
    /// Parts add up across pieces — forty parts in a programme is forty jobs to
    /// fill. **People do not**: a musician who plays every piece is one
    /// musician, and summing the per-piece counts would report them once per
    /// piece. So the player figures are distinct people across the whole
    /// collection, and "acknowledged" means they have acknowledged *everything*
    /// they hold in it — which is the question an archivist is actually asking
    /// before a concert.
    fn finish_report(report: &mut CoverageReport) {
        report.required = report.items.iter().map(|i| i.required).sum();
        report.covered = report.items.iter().map(|i| i.covered).sum();

        let mut people: Vec<(Uuid, bool)> = Vec::new();
        for item in report.items.iter().filter(|i| !i.arrangement_removed) {
            for voice in &item.voices {
                for player in &voice.players {
                    let acknowledged = player.acknowledged_at.is_some();
                    match people.iter_mut().find(|(id, _)| *id == player.user_id) {
                        Some((_, all_acked)) => *all_acked = *all_acked && acknowledged,
                        None => people.push((player.user_id, acknowledged)),
                    }
                }
            }
        }
        report.players = people.len();
        report.players_acknowledged = people.iter().filter(|(_, acked)| *acked).count();
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
    query_collection(pool, collection_id, None, required).await
}

/// Coverage for a single piece — the console's assignment screen, which shows
/// exactly this: every voice of one piece and everyone playing it.
///
/// Shares [`for_collection`]'s query rather than adding a third one. The
/// assignment screen and the coverage screens then cannot disagree about who is
/// on a part, which two hand-written joins eventually would.
pub async fn for_item(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
    required: &Required,
) -> Result<Option<ItemCoverage>, sqlx::Error> {
    let report = query_collection(pool, collection_id, Some(item_id), required).await?;
    Ok(report.items.into_iter().next())
}

async fn query_collection(
    pool: &PgPool,
    collection_id: Uuid,
    item_filter: Option<Uuid>,
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
            pa.id as "assignment_id?",
            pa.user_id as "assignee_user_id?",
            u.username as "assignee_username?",
            u.display_name as "assignee_display_name?",
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
         AND ($2::uuid[] IS NULL OR v.instrument_id = ANY($2))
        LEFT JOIN part_assignment pa
          ON pa.collection_item_id = ci.id
         AND pa.voice_id = v.id
        LEFT JOIN "user" u ON u.id = pa.user_id
        WHERE ci.collection_id = $1
          AND ci.deleted_at IS NULL
          AND c.deleted_at IS NULL
          AND ($3::uuid IS NULL OR ci.id = $3)
        ORDER BY ci.index ASC, ci.id ASC, v.name ASC, v.id ASC, pa.created_at ASC, pa.id ASC
        "#,
        collection_id,
        instrument_filter.as_deref(),
        item_filter,
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
                covered: 0,
                players: 0,
                players_acknowledged: 0,
                conflicts: Vec::new(),
            });
        }
        let item = items.last_mut().expect("just pushed");
        Grouper::push_voice(
            item,
            VoiceRow {
                voice_id: row.voice_id,
                voice_name: row.voice_name,
                instrument_id: row.instrument_id,
                assignment_id: row.assignment_id,
                user_id: row.assignee_user_id,
                username: row.assignee_username,
                display_name: row.assignee_display_name,
                notified_at: row.notified_at,
                acknowledged_at: row.acknowledged_at,
            },
        );
    }
    for item in &mut items {
        Grouper::finish_item(item);
    }

    let mut report = CoverageReport {
        collection_id,
        items,
        required: 0,
        covered: 0,
        players: 0,
        players_acknowledged: 0,
    };
    Grouper::finish_report(&mut report);
    Ok(report)
}

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
            pa.id as "assignment_id?",
            pa.user_id as "assignee_user_id?",
            u.username as "assignee_username?",
            u.display_name as "assignee_display_name?",
            pa.notified_at as "notified_at?: DateTime<Utc>",
            pa.acknowledged_at as "acknowledged_at?: DateTime<Utc>"
        FROM collection c
        LEFT JOIN collection_item ci
          ON ci.collection_id = c.id AND ci.deleted_at IS NULL
        LEFT JOIN arrangement a ON a.id = ci.arrangement_id
        LEFT JOIN voice v
          ON v.arrangement_id = a.id
         AND v.deleted_at IS NULL
         AND ($2::uuid[] IS NULL OR v.instrument_id = ANY($2))
        LEFT JOIN part_assignment pa
          ON pa.collection_item_id = ci.id AND pa.voice_id = v.id
        LEFT JOIN "user" u ON u.id = pa.user_id
        WHERE c.organization_id = $1 AND c.deleted_at IS NULL
        ORDER BY c.name ASC, c.id ASC, ci.index ASC, ci.id ASC, v.name ASC, v.id ASC,
                 pa.created_at ASC, pa.id ASC
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
                covered: 0,
                players: 0,
                players_acknowledged: 0,
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
                covered: 0,
                players: 0,
                players_acknowledged: 0,
                conflicts: Vec::new(),
            });
        }
        let item = report.items.last_mut().expect("just pushed");
        Grouper::push_voice(
            item,
            VoiceRow {
                voice_id: row.voice_id,
                voice_name: row.voice_name,
                instrument_id: row.instrument_id,
                assignment_id: row.assignment_id,
                user_id: row.assignee_user_id,
                username: row.assignee_username,
                display_name: row.assignee_display_name,
                notified_at: row.notified_at,
                acknowledged_at: row.acknowledged_at,
            },
        );
    }

    for report in &mut reports {
        for item in &mut report.items {
            Grouper::finish_item(item);
        }
        Grouper::finish_report(report);
    }
    Ok(reports)
}
