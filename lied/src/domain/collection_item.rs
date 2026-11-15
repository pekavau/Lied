//! CollectionItem: an arrangement within a collection (docs/design.md
//! Entities: CollectionItem, and *Collection numbering*). Issues #9, #102.
//!
//! Every item has a **position** (the running order, dense 1..n over the live
//! items, maintained here) and a **label** (the number players are called
//! by). What the label means depends on the collection's type:
//!
//! - a **program** is a concert: its numbers are positions, so its labels are
//!   always the positions as text, and adding, removing and reordering
//!   renumber freely;
//! - a **standing** collection (a Marschbuch, a Frühschoppenmappe) lives on
//!   paper in every player's folder: its labels (`12`, `12a`) are the pieces'
//!   identities, a removed piece leaves a gap, and only an explicit
//!   [`renumber`] closes the gaps. Its positions follow the labels' natural
//!   order ([`Label`]'s `Ord`), recomputed on every write.
//!
//! **One writer per collection.** Every write runs in one transaction that
//! first locks the collection row ([`lock_collection`]); the rules (which
//! label is free, which position exists) are checked and the rows written
//! under that lock. The live-row uniqueness of `(collection_id, position)`
//! and `(collection_id, label)` is a deferred exclusion constraint, checked
//! at commit, so a write may move items through each other's numbers.
//!
//! **Hide-with-references.** An item whose arrangement has been soft-deleted
//! is *not* dropped from the collection; it is surfaced as "removed"
//! ([`CollectionItemView::arrangement_removed`]) with the arrangement slug
//! still shown, so a broken reference is visible rather than silently
//! missing.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// Wire representation of a `collection_item` row.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItem {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub arrangement_id: Uuid,
    /// Running order, 1..n over the live items; order a list by it.
    pub position: i32,
    /// The number players are called by: `12`, or `12a` for a piece slipped
    /// in later. In a program it is always `position` as text. Display it.
    pub label: String,
    /// The section the piece's number falls into (the one with the greatest
    /// start at or below it); `null` before the first section, in a
    /// collection without sections, and always in a program.
    pub section: Option<ItemSection>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// The section an item falls into, as much as a client needs to show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemSection {
    pub id: Uuid,
    pub name: String,
}

/// A `collection_item` row joined with the section its label falls into.
///
/// Every item query finds the section with the same join (`ci` the item):
/// the section of the collection with the greatest start at or below the
/// label's number (`65a` → 65). The `sqlx` macros take a string literal, so
/// each query spells it out; keep them in step:
///
/// ```sql
/// LEFT JOIN LATERAL (
///     SELECT s.id, s.name FROM collection_section s
///     WHERE s.collection_id = ci.collection_id
///       AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
///     ORDER BY s.starts_at DESC LIMIT 1
/// ) s ON true
/// ```
struct ItemRow {
    id: Uuid,
    collection_id: Uuid,
    arrangement_id: Uuid,
    position: i32,
    label: String,
    section_id: Option<Uuid>,
    section_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    deleted_at: Option<DateTime<Utc>>,
}

impl From<ItemRow> for CollectionItem {
    fn from(r: ItemRow) -> Self {
        CollectionItem {
            id: r.id,
            collection_id: r.collection_id,
            arrangement_id: r.arrangement_id,
            position: r.position,
            label: r.label,
            section: item_section(r.section_id, r.section_name),
            created_at: r.created_at,
            updated_at: r.updated_at,
            created_by: r.created_by,
            deleted_at: r.deleted_at,
        }
    }
}

fn item_section(id: Option<Uuid>, name: Option<String>) -> Option<ItemSection> {
    Some(ItemSection {
        id: id?,
        name: name?,
    })
}

/// A collection item plus its referenced arrangement's display info, so a
/// soft-deleted ("removed") arrangement is surfaced rather than dropped.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemView {
    #[serde(flatten)]
    pub item: CollectionItem,
    pub arrangement_slug: String,
    pub arrangement_title: String,
    /// `true` when the referenced arrangement is soft-deleted — render it as
    /// "[removed]" with the slug, not as a live entry.
    pub arrangement_removed: bool,
}

// ── Labels ──────────────────────────────────────────────────────────────────

/// A piece number in a standing collection: 1–9999 with an optional
/// lowercase letter (`12`, `12a`). Ordered naturally: by number, then the
/// plain number before its letters (`12` < `12a` < `12b` < `13`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label {
    number: u16,
    letter: Option<char>,
}

/// The largest number any label can carry (four digits); a collection's
/// `number_width` may cap it lower.
const MAX_LABEL_NUMBER: u16 = 9999;

impl Label {
    /// Parse a label as a client writes it: digits without a leading zero,
    /// then at most one letter, case-insensitive (`12A` is `12a`).
    pub fn parse(text: &str) -> Option<Label> {
        let text = text.trim();
        let (digits, letter) = match text.char_indices().last() {
            Some((i, c)) if c.is_ascii_alphabetic() => (&text[..i], Some(c.to_ascii_lowercase())),
            _ => (text, None),
        };
        if digits.is_empty()
            || digits.len() > 4
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let number: u16 = digits.parse().ok()?;
        Some(Label { number, letter })
    }

    /// Parse a label as it appears in a WebDAV folder name: zero-padded
    /// (`007`, `012a`). Unpadded is accepted too.
    pub fn parse_padded(text: &str) -> Option<Label> {
        let trimmed = text.trim_start_matches('0');
        Label::parse(trimmed)
    }

    /// The plain number `n`.
    pub fn plain(number: u16) -> Label {
        Label {
            number,
            letter: None,
        }
    }

    pub fn number(&self) -> u16 {
        self.number
    }

    pub fn letter(&self) -> Option<char> {
        self.letter
    }

    /// Whether the number fits `width` digits.
    pub fn fits(&self, width: i16) -> bool {
        self.number <= max_number(width)
    }

    /// Zero-padded to `width` digits, for a WebDAV folder name (`007`,
    /// `012a`, `0007`).
    pub fn padded(&self, width: i16) -> String {
        let width = usize::try_from(width).unwrap_or(3);
        match self.letter {
            Some(letter) => format!("{:0width$}{letter}", self.number),
            None => format!("{:0width$}", self.number),
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.letter {
            Some(letter) => write!(f, "{}{letter}", self.number),
            None => write!(f, "{}", self.number),
        }
    }
}

/// The highest number a collection of `width` digits can label.
pub fn max_number(width: i16) -> u16 {
    match width {
        3 => 999,
        _ => MAX_LABEL_NUMBER,
    }
}

/// The WebDAV folder name of an item: its label padded to the collection's
/// width, then the arrangement slug (`007-radetzky-march`). A label that does
/// not parse (cannot happen: the column has a format CHECK) is used as is.
pub fn folder_name(label: &str, width: i16, arrangement_slug: &str) -> String {
    let label = Label::parse(label).map_or_else(|| label.to_string(), |l| l.padded(width));
    format!("{label}-{arrangement_slug}")
}

/// Why a label cannot be used for a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelRefusal {
    /// A live piece already has it.
    Taken,
    /// Its number has more digits than the collection's `number_width`.
    TooWide,
    /// Not one of the allowed labels: a number past the end, or a letter
    /// next to a number nobody has.
    NotAllowed,
}

/// A section of a standing collection, as the numbering rule sees it: a
/// heading whose range runs from `starts_at` to the next section's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionBound {
    pub id: Uuid,
    pub name: String,
    pub starts_at: u16,
}

/// The numbering of a standing collection's live pieces, and the rule for
/// which label a piece may take (docs/design.md, *Which labels a new piece
/// may take*).
///
/// The rule works per **range**: each section covers its start up to the
/// next section's start, and the numbers before the first section form one
/// more range (without sections, the whole collection is one range from 1).
/// Within a range, a new piece takes a gap below the range's highest number
/// or the number after it; a range without pieces offers its start.
#[derive(Debug, Clone)]
pub struct Numbering {
    labels: BTreeSet<Label>,
    width: i16,
    /// Sorted by `starts_at`.
    sections: Vec<SectionBound>,
}

/// The labels a new piece can choose from, for a form to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChoices {
    /// Highest number + 1 (or `1` in an empty collection); `None` when that
    /// no longer fits the collection's width.
    pub suggested: Option<Label>,
    /// Free numbers below the highest one of their range, ascending.
    pub gaps: Vec<Label>,
    /// Per range, the next number: one entry per section, plus one for the
    /// numbers before the first section when that section does not start at
    /// 1. Empty when the collection has no sections.
    pub sections: Vec<SectionChoice>,
}

/// The next number a range offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionChoice {
    /// `None` for the numbers before the first section.
    pub section: Option<SectionBound>,
    pub starts_at: u16,
    /// The range's start while it holds no piece, else its highest number +
    /// 1; `None` when that would be the next section's start (the section
    /// is **full**: take a gap, a letter label, or renumber) or past the
    /// collection's width.
    pub next: Option<Label>,
}

/// One range of numbers, `[start, end)`; `end` is the next section's start.
struct Range<'a> {
    section: Option<&'a SectionBound>,
    start: u16,
    end: Option<u16>,
}

impl Numbering {
    pub fn new(labels: impl IntoIterator<Item = Label>, width: i16) -> Self {
        Numbering {
            labels: labels.into_iter().collect(),
            width,
            sections: Vec::new(),
        }
    }

    /// The same numbering, divided into `sections`.
    pub fn with_sections(mut self, sections: impl IntoIterator<Item = SectionBound>) -> Self {
        self.sections = sections.into_iter().collect();
        self.sections.sort_by_key(|s| s.starts_at);
        self
    }

    /// The same numbering without `label`: for moving a piece, which must
    /// not count its own current label against itself.
    pub fn without(&self, label: &Label) -> Self {
        let mut labels = self.labels.clone();
        labels.remove(label);
        Numbering {
            labels,
            width: self.width,
            sections: self.sections.clone(),
        }
    }

    fn highest_number(&self) -> u16 {
        self.labels.iter().map(Label::number).max().unwrap_or(0)
    }

    fn number_in_use(&self, number: u16) -> bool {
        self.labels.iter().any(|l| l.number == number)
    }

    fn ranges(&self) -> Vec<Range<'_>> {
        let mut ranges = Vec::with_capacity(self.sections.len() + 1);
        let first = self.sections.first().map(|s| s.starts_at);
        if first.is_none_or(|start| start > 1) {
            ranges.push(Range {
                section: None,
                start: 1,
                end: first,
            });
        }
        for (i, section) in self.sections.iter().enumerate() {
            ranges.push(Range {
                section: Some(section),
                start: section.starts_at,
                end: self.sections.get(i + 1).map(|s| s.starts_at),
            });
        }
        ranges
    }

    fn range_of(&self, number: u16) -> Range<'_> {
        self.ranges()
            .into_iter()
            .rev()
            .find(|r| r.start <= number)
            // Unreachable: the first range starts at 1 and numbers are ≥ 1.
            .unwrap_or(Range {
                section: None,
                start: 1,
                end: None,
            })
    }

    fn highest_in(&self, range: &Range<'_>) -> Option<u16> {
        self.labels
            .iter()
            .map(Label::number)
            .filter(|n| *n >= range.start && range.end.is_none_or(|end| *n < end))
            .max()
    }

    /// Whether `candidate` may be given to a piece:
    /// - a free plain number below the highest one of its range (a gap), or
    ///   that highest + 1; in a range without pieces, its start (`1` in an
    ///   empty collection without sections);
    /// - a free letter label `Na` whose number `N` some live piece carries
    ///   (`12b` needs a `12` or `12a`, not both).
    ///
    /// Everything else is refused: a number far past the end of its range
    /// is usually a typo, and would leave a gap nobody chose. Starting a
    /// section past the end is what makes a far number allowed.
    pub fn check(&self, candidate: Label) -> Result<(), LabelRefusal> {
        if !candidate.fits(self.width) {
            return Err(LabelRefusal::TooWide);
        }
        if self.labels.contains(&candidate) {
            return Err(LabelRefusal::Taken);
        }
        let allowed = match candidate.letter {
            None => {
                let range = self.range_of(candidate.number);
                match self.highest_in(&range) {
                    None => candidate.number == range.start,
                    Some(highest) => candidate.number <= highest + 1,
                }
            }
            Some(_) => self.number_in_use(candidate.number),
        };
        if allowed {
            Ok(())
        } else {
            Err(LabelRefusal::NotAllowed)
        }
    }

    /// The suggestion, the gaps and each range's next number, for a form.
    pub fn choices(&self) -> LabelChoices {
        let next = Label::plain(self.highest_number() + 1);
        let mut gaps = Vec::new();
        let mut sections = Vec::new();
        for range in self.ranges() {
            let highest = self.highest_in(&range);
            if let Some(highest) = highest {
                gaps.extend(
                    (range.start..highest)
                        .map(Label::plain)
                        .filter(|l| !self.labels.contains(l)),
                );
            }
            if !self.sections.is_empty() {
                let candidate = Label::plain(highest.map_or(range.start, |h| h + 1));
                let in_range = range.end.is_none_or(|end| candidate.number < end);
                sections.push(SectionChoice {
                    section: range.section.cloned(),
                    starts_at: range.start,
                    next: (in_range && candidate.fits(self.width)).then_some(candidate),
                });
            }
        }
        LabelChoices {
            suggested: self.check(next).is_ok().then_some(next),
            gaps,
            sections,
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum CollectionItemError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// The collection does not exist or is soft-deleted.
    #[error("the collection does not exist")]
    CollectionGone,
    #[error("the referenced collection or arrangement does not exist")]
    UnknownReference,
    #[error("the item order must be exactly the collection's current live items")]
    InvalidReorder,
    /// This arrangement is already a live piece in the collection. Adding it
    /// twice — or restoring a removed piece after re-adding the same
    /// arrangement — would list the same music under two numbers.
    #[error("this arrangement is already in the collection")]
    DuplicateArrangement,
    /// A field that the collection's type does not use: `label` on a
    /// program, `position` on a standing collection.
    #[error("{field} does not apply to a {collection_type} collection")]
    WrongField {
        field: &'static str,
        collection_type: &'static str,
    },
    /// A position outside `1..=max`.
    #[error("position must be between 1 and {max}")]
    InvalidPosition { max: i32 },
    /// Not a label: digits without a leading zero, then at most one letter.
    #[error("a label is a number from 1 to 9999, optionally followed by one letter (12, 12a)")]
    InvalidLabel,
    /// A well-formed label the rule refuses (see [`Numbering::check`]).
    #[error("label {label} is not allowed: {}", refusal_text(*refusal))]
    LabelRefused { label: Label, refusal: LabelRefusal },
    /// Restoring a piece whose old label is taken (or no longer fits) needs
    /// a label chosen.
    #[error("label {old} is no longer free; choose a label to restore this piece under")]
    LabelNeeded { old: String },
    /// No room for another piece at the collection's number width.
    #[error("the collection is full at its number width; widen it or renumber")]
    CollectionFull,
    /// Renumber and label choices are for standing collections; a
    /// program's numbers are its running order, always dense.
    #[error("only a standing collection has labels to choose or renumber; a program's numbers are its running order")]
    NotStanding,
    /// Reorder is for programs; a standing collection's order follows its
    /// labels.
    #[error("a standing collection's order follows its labels; relabel, swap or renumber instead")]
    NotProgram,
    /// The collection changed between the renumber preview and the apply.
    #[error("the collection changed since the preview; preview the renumber again")]
    StaleRenumber,
    /// A swap of a piece with itself.
    #[error("a piece cannot be swapped with itself")]
    SameItem,
}

fn refusal_text(refusal: LabelRefusal) -> &'static str {
    match refusal {
        LabelRefusal::Taken => "another piece has it",
        LabelRefusal::TooWide => "it has more digits than the collection's number width",
        LabelRefusal::NotAllowed => {
            "choose a free number below the highest, the highest + 1, or a letter after an existing number"
        }
    }
}

fn map_write_error(err: sqlx::Error) -> CollectionItemError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("collection_item_collection_id_arrangement_id_key") {
            return CollectionItemError::DuplicateArrangement;
        }
        if db_err.is_foreign_key_violation() {
            return CollectionItemError::UnknownReference;
        }
    }
    CollectionItemError::Database(err)
}

// ── The collection lock and the layout ─────────────────────────────────────

/// A collection's type, as it decides the numbering rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Program,
    Standing,
}

impl Kind {
    pub fn parse(collection_type: &str) -> Kind {
        if collection_type == "standing" {
            Kind::Standing
        } else {
            Kind::Program
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Program => "program",
            Kind::Standing => "standing",
        }
    }
}

/// What a write needs to know about the collection it holds the lock on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Locked {
    pub kind: Kind,
    pub number_width: i16,
}

/// Lock a live collection's row for an item write, so writes to one
/// collection run one at a time. `FOR NO KEY UPDATE` still lets other rows
/// insert references to the collection.
pub(crate) async fn lock_collection(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
) -> Result<Locked, CollectionItemError> {
    let row = sqlx::query!(
        r#"SELECT type as "collection_type!", number_width FROM collection
           WHERE id = $1 AND deleted_at IS NULL
           FOR NO KEY UPDATE"#,
        collection_id,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(CollectionItemError::CollectionGone)?;
    Ok(Locked {
        kind: Kind::parse(&row.collection_type),
        number_width: row.number_width,
    })
}

/// One live item as the layout code sees it.
#[derive(Debug, Clone)]
struct Slot {
    id: Uuid,
    label: Label,
}

/// The live items in position order.
async fn live_slots(tx: &mut Tx<'_>, collection_id: Uuid) -> Result<Vec<Slot>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id, label FROM collection_item
           WHERE collection_id = $1 AND deleted_at IS NULL
           ORDER BY position ASC, id ASC"#,
        collection_id,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Slot {
            id: r.id,
            // The column's CHECK keeps every label parseable.
            label: Label::parse(&r.label).unwrap_or(Label::plain(MAX_LABEL_NUMBER)),
        })
        .collect())
}

/// Write positions 1..n in `order`, with labels: a program's are the
/// positions; a standing collection's are the slots' own. Rows already at
/// their place are left alone (their `updated_at`, and so their ETag, stays).
async fn write_layout(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    kind: Kind,
    order: &[Slot],
) -> Result<(), CollectionItemError> {
    for (index, slot) in order.iter().enumerate() {
        let position = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let label = match kind {
            Kind::Program => position.to_string(),
            Kind::Standing => slot.label.to_string(),
        };
        sqlx::query!(
            r#"UPDATE collection_item SET position = $2, label = $3, updated_at = now()
               WHERE id = $1 AND collection_id = $4 AND deleted_at IS NULL
                 AND (position <> $2 OR label <> $3)"#,
            slot.id,
            position,
            label,
            collection_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(map_write_error)?;
    }
    Ok(())
}

/// A collection's sections, by start.
pub(crate) async fn live_sections(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
) -> Result<Vec<SectionBound>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id, name, starts_at FROM collection_section
           WHERE collection_id = $1
           ORDER BY starts_at ASC"#,
        collection_id,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SectionBound {
            id: r.id,
            name: r.name,
            // The column's CHECK keeps it in 1..=9999.
            starts_at: u16::try_from(r.starts_at).unwrap_or(MAX_LABEL_NUMBER),
        })
        .collect())
}

/// The numbering of a standing collection's live pieces and its sections,
/// under the collection lock.
async fn numbering_of(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    width: i16,
    slots: &[Slot],
) -> Result<Numbering, sqlx::Error> {
    let sections = live_sections(tx, collection_id).await?;
    Ok(Numbering::new(slots.iter().map(|s| s.label), width).with_sections(sections))
}

/// The labels a new piece of a standing collection can take: the
/// suggestion, the gaps and each section's next number. A program has none
/// ([`CollectionItemError::NotStanding`]).
pub async fn label_choices(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<LabelChoices, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    if locked.kind != Kind::Standing {
        return Err(CollectionItemError::NotStanding);
    }
    let slots = live_slots(&mut tx, collection_id).await?;
    let numbering = numbering_of(&mut tx, collection_id, locked.number_width, &slots).await?;
    tx.rollback().await?;
    Ok(numbering.choices())
}

/// Sort a standing collection's slots by label, the order its positions
/// follow.
fn by_label(mut slots: Vec<Slot>) -> Vec<Slot> {
    slots.sort_by_key(|s| s.label);
    slots
}

/// Commit, mapping a constraint checked at commit (the deferred uniqueness
/// backstop) like any other write error.
async fn commit(tx: Tx<'_>) -> Result<(), CollectionItemError> {
    tx.commit().await.map_err(map_write_error)
}

fn parse_label(text: &str) -> Result<Label, CollectionItemError> {
    Label::parse(text).ok_or(CollectionItemError::InvalidLabel)
}

fn refused(label: Label, refusal: LabelRefusal) -> CollectionItemError {
    CollectionItemError::LabelRefused { label, refusal }
}

/// Whether a program of `count` items still fits its width (its labels are
/// its positions).
fn program_fits(count: usize, width: i16) -> bool {
    count <= usize::from(max_number(width))
}

async fn fetch_item(tx: &mut Tx<'_>, id: Uuid) -> Result<CollectionItem, sqlx::Error> {
    sqlx::query_as!(
        ItemRow,
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            s.id as "section_id?", s.name as "section_name?",
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item ci
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.id = $1
        "#,
        id,
    )
    .fetch_one(&mut **tx)
    .await
    .map(CollectionItem::from)
}

// ── Reads ───────────────────────────────────────────────────────────────────

/// Look up a live item by id (soft-deleted hidden).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<CollectionItem>, sqlx::Error> {
    Ok(sqlx::query_as!(
        ItemRow,
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            s.id as "section_id?", s.name as "section_name?",
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item ci
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.id = $1 AND ci.deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?
    .map(CollectionItem::from))
}

/// Look up an item by id including soft-deleted rows (undelete flow).
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<CollectionItem>, sqlx::Error> {
    Ok(sqlx::query_as!(
        ItemRow,
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            s.id as "section_id?", s.name as "section_name?",
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item ci
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?
    .map(CollectionItem::from))
}

/// List a collection's live items in position order, each with its
/// arrangement's display info (surfacing soft-deleted arrangements as
/// removed). Items are hidden if the collection itself is soft-deleted.
///
/// **Deliberately unpaginated.** Every caller needs the whole set to be
/// correct, not merely complete: the console renders one reorder form over all
/// rows and submits their ids as the new order, and the domain's [`reorder`]
/// rejects anything that is not a permutation of the live items — so a page of
/// them would be refused by construction. The bound is the collection itself.
pub async fn list_for_collection(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<Vec<CollectionItemView>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>",
            s.id as "section_id?", s.name as "section_name?",
            a.slug as arrangement_slug,
            a.title as arrangement_title,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
        ORDER BY ci.position ASC, ci.id ASC
        "#,
        collection_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CollectionItemView {
            item: CollectionItem {
                id: r.id,
                collection_id: r.collection_id,
                arrangement_id: r.arrangement_id,
                position: r.position,
                label: r.label,
                section: item_section(r.section_id, r.section_name),
                created_at: r.created_at,
                updated_at: r.updated_at,
                created_by: r.created_by,
                deleted_at: r.deleted_at,
            },
            arrangement_slug: r.arrangement_slug,
            arrangement_title: r.arrangement_title,
            arrangement_removed: r.arrangement_removed,
        })
        .collect())
}

/// A collection's *restorable* removed items, most-recently-removed first, plus
/// the total. Backs the console's "removed pieces" list.
///
/// Two filters make the list mean "these can come back":
///   - an arrangement that is live in the collection again is excluded — its
///     old row can never be restored ([`restore`] refuses it), so offering a
///     Restore button that always 409s would be a lie;
///   - where the same arrangement was removed more than once, only the most
///     recent removal is listed; restoring an older one would be restoring the
///     same piece by a different row.
///
/// Paginated so the query stays bounded however often a program has been
/// rebuilt.
pub async fn list_deleted_for_collection(
    pool: &PgPool,
    collection_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CollectionItemView>, i64), sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>",
            s.id as "section_id?", s.name as "section_name?",
            a.slug as arrangement_slug,
            a.title as arrangement_title,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NOT NULL AND c.deleted_at IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM collection_item live
              WHERE live.collection_id = ci.collection_id
                AND live.arrangement_id = ci.arrangement_id
                AND live.deleted_at IS NULL
          )
          AND ci.deleted_at = (
              SELECT max(newer.deleted_at) FROM collection_item newer
              WHERE newer.collection_id = ci.collection_id
                AND newer.arrangement_id = ci.arrangement_id
                AND newer.deleted_at IS NOT NULL
          )
        ORDER BY ci.deleted_at DESC, ci.id ASC
        LIMIT $2 OFFSET $3
        "#,
        collection_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) as "count!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NOT NULL AND c.deleted_at IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM collection_item live
              WHERE live.collection_id = ci.collection_id
                AND live.arrangement_id = ci.arrangement_id
                AND live.deleted_at IS NULL
          )
          AND ci.deleted_at = (
              SELECT max(newer.deleted_at) FROM collection_item newer
              WHERE newer.collection_id = ci.collection_id
                AND newer.arrangement_id = ci.arrangement_id
                AND newer.deleted_at IS NOT NULL
          )
        "#,
        collection_id,
    )
    .fetch_one(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| CollectionItemView {
            item: CollectionItem {
                id: r.id,
                collection_id: r.collection_id,
                arrangement_id: r.arrangement_id,
                position: r.position,
                label: r.label,
                section: item_section(r.section_id, r.section_name),
                created_at: r.created_at,
                updated_at: r.updated_at,
                created_by: r.created_by,
                deleted_at: r.deleted_at,
            },
            arrangement_slug: r.arrangement_slug,
            arrangement_title: r.arrangement_title,
            arrangement_removed: r.arrangement_removed,
        })
        .collect();

    Ok((items, total))
}

// ── Writes ──────────────────────────────────────────────────────────────────

/// Where a new or moved piece goes. Which field applies depends on the
/// collection's type: `position` for a program, `label` for a standing
/// collection; giving the other is [`CollectionItemError::WrongField`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Placement<'a> {
    pub position: Option<i32>,
    pub label: Option<&'a str>,
}

fn check_fields(kind: Kind, placement: Placement<'_>) -> Result<(), CollectionItemError> {
    match kind {
        Kind::Program if placement.label.is_some() => Err(CollectionItemError::WrongField {
            field: "label",
            collection_type: kind.name(),
        }),
        Kind::Standing if placement.position.is_some() => Err(CollectionItemError::WrongField {
            field: "position",
            collection_type: kind.name(),
        }),
        _ => Ok(()),
    }
}

fn index_for_position(position: i32, max: usize) -> Result<usize, CollectionItemError> {
    let max_i32 = i32::try_from(max).unwrap_or(i32::MAX);
    if (1..=max_i32).contains(&position) {
        Ok(usize::try_from(position - 1).unwrap_or(0))
    } else {
        Err(CollectionItemError::InvalidPosition { max: max_i32 })
    }
}

/// Add an arrangement to a collection.
///
/// - Program: at `placement.position` (1..n+1, default the end); the pieces
///   from there on move down one.
/// - Standing: under `placement.label` (an allowed label, see
///   [`Numbering::check`]; default highest + 1); nothing else moves.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    collection_id: Uuid,
    arrangement_id: Uuid,
    placement: Placement<'_>,
    created_by: Option<Uuid>,
) -> Result<CollectionItem, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    check_fields(locked.kind, placement)?;
    let slots = live_slots(&mut tx, collection_id).await?;

    let (label, order) = match locked.kind {
        Kind::Program => {
            if !program_fits(slots.len() + 1, locked.number_width) {
                return Err(CollectionItemError::CollectionFull);
            }
            let at = match placement.position {
                Some(position) => index_for_position(position, slots.len() + 1)?,
                None => slots.len(),
            };
            let mut order = slots;
            // The label is rewritten by `write_layout`; any value will do.
            let label = Label::plain(1);
            order.insert(at, Slot { id, label });
            (label, order)
        }
        Kind::Standing => {
            let numbering =
                numbering_of(&mut tx, collection_id, locked.number_width, &slots).await?;
            let label = match placement.label {
                Some(text) => {
                    let label = parse_label(text)?;
                    numbering.check(label).map_err(|r| refused(label, r))?;
                    label
                }
                None => numbering
                    .choices()
                    .suggested
                    .ok_or(CollectionItemError::CollectionFull)?,
            };
            let mut order = slots;
            order.push(Slot { id, label });
            (label, by_label(order))
        }
    };

    // Inserted at the end; `write_layout` then gives every piece its place
    // (the uniqueness check waits for the commit).
    let end = i32::try_from(order.len()).unwrap_or(i32::MAX);
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, position, label, created_by)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
        id,
        collection_id,
        arrangement_id,
        end,
        label.to_string(),
        created_by,
    )
    .execute(&mut *tx)
    .await
    .map_err(map_write_error)?;
    write_layout(&mut tx, collection_id, locked.kind, &order).await?;
    let created = fetch_item(&mut tx, id).await?;
    commit(tx).await?;
    Ok(created)
}

/// Lock a live item of the collection at the caller's version, under the
/// collection lock. `None`: no such live item, or a stale version.
async fn lock_item(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    item_id: Uuid,
    expected_version: Option<i64>,
) -> Result<Option<CollectionItem>, sqlx::Error> {
    Ok(sqlx::query_as!(
        ItemRow,
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.position, ci.label,
            s.id as "section_id?", s.name as "section_name?",
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item ci
        LEFT JOIN LATERAL (
            SELECT s.id, s.name FROM collection_section s
            WHERE s.collection_id = ci.collection_id
              AND s.starts_at <= rtrim(ci.label, 'abcdefghijklmnopqrstuvwxyz')::int
            ORDER BY s.starts_at DESC LIMIT 1
        ) s ON true
        WHERE ci.id = $1 AND ci.collection_id = $2 AND ci.deleted_at IS NULL
          AND ($3::bigint IS NULL OR floor(extract(epoch FROM ci.updated_at) * 1000)::bigint = $3)
        FOR UPDATE OF ci
        "#,
        item_id,
        collection_id,
        expected_version,
    )
    .fetch_optional(&mut **tx)
    .await?
    .map(CollectionItem::from))
}

/// Move a piece.
///
/// - Program: to `placement.position` (1..n); the pieces in between shift by
///   one.
/// - Standing: to `placement.label`, an allowed label (its own current label
///   does not count against it); nothing else moves.
///
/// Moving a piece to where it already is writes nothing. Returns `None` if
/// no live item matched at `expected_version`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn move_item(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
    expected_version: Option<i64>,
    placement: Placement<'_>,
) -> Result<Option<CollectionItem>, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    check_fields(locked.kind, placement)?;
    let Some(current) = lock_item(&mut tx, collection_id, item_id, expected_version).await? else {
        return Ok(None);
    };
    let slots = live_slots(&mut tx, collection_id).await?;

    let order = match locked.kind {
        Kind::Program => {
            let Some(position) = placement.position else {
                return Ok(Some(current));
            };
            let to = index_for_position(position, slots.len())?;
            if position == current.position {
                return Ok(Some(current));
            }
            let mut order: Vec<Slot> = slots;
            let from = order.iter().position(|s| s.id == item_id).unwrap_or(0);
            let slot = order.remove(from);
            order.insert(to, slot);
            order
        }
        Kind::Standing => {
            let Some(text) = placement.label else {
                return Ok(Some(current));
            };
            let label = parse_label(text)?;
            let own = Label::parse(&current.label).unwrap_or(Label::plain(MAX_LABEL_NUMBER));
            if label == own {
                return Ok(Some(current));
            }
            let numbering = numbering_of(&mut tx, collection_id, locked.number_width, &slots)
                .await?
                .without(&own);
            numbering.check(label).map_err(|r| refused(label, r))?;
            let order = slots
                .into_iter()
                .map(|s| {
                    if s.id == item_id {
                        Slot { id: s.id, label }
                    } else {
                        s
                    }
                })
                .collect();
            by_label(order)
        }
    };
    write_layout(&mut tx, collection_id, locked.kind, &order).await?;
    let moved = fetch_item(&mut tx, item_id).await?;
    commit(tx).await?;
    Ok(Some(moved))
}

/// Swap two pieces: their positions in a program, their labels in a standing
/// collection. Returns `false` if either is not a live item of the
/// collection.
pub async fn swap(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
    with_item_id: Uuid,
) -> Result<bool, CollectionItemError> {
    if item_id == with_item_id {
        return Err(CollectionItemError::SameItem);
    }
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    let mut slots = live_slots(&mut tx, collection_id).await?;
    let (Some(a), Some(b)) = (
        slots.iter().position(|s| s.id == item_id),
        slots.iter().position(|s| s.id == with_item_id),
    ) else {
        return Ok(false);
    };
    let order = match locked.kind {
        Kind::Program => {
            slots.swap(a, b);
            slots
        }
        Kind::Standing => {
            let (label_a, label_b) = (slots[a].label, slots[b].label);
            slots[a].label = label_b;
            slots[b].label = label_a;
            by_label(slots)
        }
    };
    write_layout(&mut tx, collection_id, locked.kind, &order).await?;
    commit(tx).await?;
    Ok(true)
}

/// Reassign a program's positions to `1..=n` in the order of `ordered_ids`,
/// atomically. `ordered_ids` must be *exactly* a permutation of the
/// collection's current live item ids — no missing, extra, foreign, or
/// duplicate ids — else [`CollectionItemError::InvalidReorder`] (a partial
/// list would otherwise silently corrupt the order).
///
/// A standing collection's order follows its labels, so it refuses with
/// [`CollectionItemError::NotProgram`].
pub async fn reorder(
    pool: &PgPool,
    collection_id: Uuid,
    ordered_ids: &[Uuid],
) -> Result<(), CollectionItemError> {
    use std::collections::HashSet;

    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    if locked.kind != Kind::Program {
        return Err(CollectionItemError::NotProgram);
    }
    let slots = live_slots(&mut tx, collection_id).await?;
    let live_set: HashSet<Uuid> = slots.iter().map(|s| s.id).collect();
    let ordered_set: HashSet<Uuid> = ordered_ids.iter().copied().collect();
    if ordered_ids.len() != slots.len() || ordered_set != live_set {
        return Err(CollectionItemError::InvalidReorder);
    }
    let order: Vec<Slot> = ordered_ids
        .iter()
        .map(|id| Slot {
            id: *id,
            label: Label::plain(1),
        })
        .collect();
    write_layout(&mut tx, collection_id, locked.kind, &order).await?;
    commit(tx).await
}

/// Remove a piece. In a program the gap closes (the pieces after it move up
/// one); in a standing collection **the gap stays**: the other labels are
/// printed on paper and do not change. Positions stay dense either way.
///
/// Returns `false` if no live item matched at `expected_version`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn remove(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
    expected_version: Option<i64>,
) -> Result<bool, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    if lock_item(&mut tx, collection_id, item_id, expected_version)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = now(), updated_at = now() WHERE id = $1"#,
        item_id,
    )
    .execute(&mut *tx)
    .await?;
    let remaining = live_slots(&mut tx, collection_id).await?;
    write_layout(&mut tx, collection_id, locked.kind, &remaining).await?;
    commit(tx).await?;
    Ok(true)
}

/// Restore a removed piece.
///
/// - Program: at the position it held, the pieces from there on moving down
///   one; at the end if the collection has since shrunk below it.
/// - Standing: under its old label if that is still free (and fits the
///   width), else under `label`, an allowed label; with neither,
///   [`CollectionItemError::LabelNeeded`].
///
/// Refuses with [`CollectionItemError::DuplicateArrangement`] when the same
/// arrangement has been re-added in the meantime: the collection would
/// otherwise list one piece twice.
///
/// Returns `false` if no removed item matched.
pub async fn restore(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
    label: Option<&str>,
) -> Result<bool, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    check_fields(
        locked.kind,
        Placement {
            position: None,
            label,
        },
    )?;

    let Some(target) = sqlx::query!(
        r#"SELECT position, label, arrangement_id FROM collection_item
           WHERE id = $1 AND collection_id = $2 AND deleted_at IS NOT NULL
           FOR UPDATE"#,
        item_id,
        collection_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(false);
    };

    // The same arrangement may have been added again while this one was gone.
    let clash = sqlx::query_scalar!(
        r#"SELECT EXISTS(
               SELECT 1 FROM collection_item
               WHERE collection_id = $1 AND arrangement_id = $2 AND deleted_at IS NULL
           ) AS "clash!""#,
        collection_id,
        target.arrangement_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if clash {
        return Err(CollectionItemError::DuplicateArrangement);
    }

    let slots = live_slots(&mut tx, collection_id).await?;
    let (new_label, order) = match locked.kind {
        Kind::Program => {
            if !program_fits(slots.len() + 1, locked.number_width) {
                return Err(CollectionItemError::CollectionFull);
            }
            let at = usize::try_from(target.position.max(1) - 1)
                .unwrap_or(0)
                .min(slots.len());
            let mut order = slots;
            order.insert(
                at,
                Slot {
                    id: item_id,
                    label: Label::plain(1),
                },
            );
            (Label::plain(1), order)
        }
        Kind::Standing => {
            let numbering =
                numbering_of(&mut tx, collection_id, locked.number_width, &slots).await?;
            let old = Label::parse(&target.label);
            let old_is_free = old.is_some_and(|l| {
                !matches!(
                    numbering.check(l),
                    Err(LabelRefusal::Taken | LabelRefusal::TooWide)
                )
            });
            let chosen = match (label, old) {
                // The old label is an undo, not a new number: it is taken
                // back even past the end, as long as it is free.
                (None, Some(old)) if old_is_free => old,
                (None, _) => {
                    return Err(CollectionItemError::LabelNeeded {
                        old: target.label.clone(),
                    })
                }
                (Some(text), old) => {
                    let wanted = parse_label(text)?;
                    if old == Some(wanted) && old_is_free {
                        wanted
                    } else {
                        numbering.check(wanted).map_err(|r| refused(wanted, r))?;
                        wanted
                    }
                }
            };
            let mut order = slots;
            order.push(Slot {
                id: item_id,
                label: chosen,
            });
            (chosen, by_label(order))
        }
    };

    let end = i32::try_from(order.len()).unwrap_or(i32::MAX);
    sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = NULL, position = $2, label = $3, updated_at = now()
           WHERE id = $1"#,
        item_id,
        end,
        new_label.to_string(),
    )
    .execute(&mut *tx)
    .await
    .map_err(map_write_error)?;
    write_layout(&mut tx, collection_id, locked.kind, &order).await?;
    commit(tx).await?;
    Ok(true)
}

// ── Renumber ────────────────────────────────────────────────────────────────

/// One piece's number before and after a renumber.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenumberChange {
    pub item_id: Uuid,
    pub from: String,
    pub to: String,
}

/// One section's start before and after a renumber.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SectionRenumberChange {
    pub section_id: Uuid,
    pub from: i32,
    pub to: i32,
}

/// What a renumber would do, and the token that applies exactly that.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenumberPreview {
    /// Every live piece, in order, with its label now and after the
    /// renumber (equal where it does not change).
    pub changes: Vec<RenumberChange>,
    /// Every section, in order, with its start now and after the renumber.
    pub section_changes: Vec<SectionRenumberChange>,
    /// Pass it back to apply this preview; it no longer matches once the
    /// collection's pieces, labels or sections change.
    pub preview_token: String,
}

/// The token for a collection's current numbering: a hash of every live
/// (item id, label) in order and every (section id, start).
fn preview_token(slots: &[Slot], sections: &[SectionBound]) -> String {
    let mut hasher = Sha256::new();
    for slot in slots {
        hasher.update(slot.id.as_bytes());
        hasher.update(slot.label.to_string().as_bytes());
        hasher.update(b";");
    }
    hasher.update(b"|");
    for section in sections {
        hasher.update(section.id.as_bytes());
        hasher.update(section.starts_at.to_be_bytes());
        hasher.update(b";");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What a renumber does: the pieces in their order, and the new numbers.
struct RenumberPlan {
    order: Vec<Slot>,
    changes: Vec<RenumberChange>,
    section_changes: Vec<SectionRenumberChange>,
}

/// Plan a renumber of a standing collection: the pieces get 1..n in their
/// order, letters dropped; each section starts at the new number of its
/// first piece. A section without pieces keeps one number, its start, right
/// after the pieces before it, so the sections stay in order and every
/// start stays distinct (two empty sections at the end become last + 1 and
/// last + 2). `slots` are in label order, `sections` by start.
fn plan_renumber(
    slots: &[Slot],
    sections: &[SectionBound],
    width: i16,
) -> Result<RenumberPlan, CollectionItemError> {
    let mut next: u32 = 1;
    let mut order = Vec::with_capacity(slots.len());
    let mut changes = Vec::with_capacity(slots.len());
    let mut section_changes = Vec::with_capacity(sections.len());
    let mut rest = slots.iter().peekable();

    let mut take_until = |end: Option<u16>, next: &mut u32| -> bool {
        let mut took = false;
        while let Some(slot) = rest.next_if(|s| end.is_none_or(|e| s.label.number() < e)) {
            let to = Label::plain(u16::try_from(*next).unwrap_or(u16::MAX));
            changes.push(RenumberChange {
                item_id: slot.id,
                from: slot.label.to_string(),
                to: to.to_string(),
            });
            order.push(Slot {
                id: slot.id,
                label: to,
            });
            *next += 1;
            took = true;
        }
        took
    };

    // The pieces before the first section.
    take_until(sections.first().map(|s| s.starts_at), &mut next);
    for (i, section) in sections.iter().enumerate() {
        let start = next;
        if !take_until(sections.get(i + 1).map(|s| s.starts_at), &mut next) {
            next += 1;
        }
        section_changes.push(SectionRenumberChange {
            section_id: section.id,
            from: i32::from(section.starts_at),
            to: i32::try_from(start).unwrap_or(i32::MAX),
        });
    }

    if next - 1 > u32::from(max_number(width)) {
        return Err(CollectionItemError::CollectionFull);
    }
    Ok(RenumberPlan {
        order,
        changes,
        section_changes,
    })
}

/// Preview a renumber of a standing collection (see [`plan_renumber`]).
/// Programs are always dense, so they refuse with
/// [`CollectionItemError::NotStanding`].
pub async fn renumber_preview(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<RenumberPreview, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    if locked.kind != Kind::Standing {
        return Err(CollectionItemError::NotStanding);
    }
    let slots = live_slots(&mut tx, collection_id).await?;
    let sections = live_sections(&mut tx, collection_id).await?;
    tx.rollback().await?;
    let plan = plan_renumber(&slots, &sections, locked.number_width)?;
    Ok(RenumberPreview {
        changes: plan.changes,
        section_changes: plan.section_changes,
        preview_token: preview_token(&slots, &sections),
    })
}

/// What an applied renumber changed.
#[derive(Debug, Clone)]
pub struct RenumberOutcome {
    pub changes: Vec<RenumberChange>,
    pub section_changes: Vec<SectionRenumberChange>,
}

/// Apply the renumber a [`renumber_preview`] showed, if the collection has
/// not changed since ([`CollectionItemError::StaleRenumber`] otherwise).
/// Returns the changes made. This is the one operation that changes a
/// standing collection's printed numbers: the re-sleeve of the folder.
pub async fn renumber(
    pool: &PgPool,
    collection_id: Uuid,
    token: &str,
) -> Result<RenumberOutcome, CollectionItemError> {
    let mut tx = pool.begin().await?;
    let locked = lock_collection(&mut tx, collection_id).await?;
    if locked.kind != Kind::Standing {
        return Err(CollectionItemError::NotStanding);
    }
    let slots = live_slots(&mut tx, collection_id).await?;
    let sections = live_sections(&mut tx, collection_id).await?;
    if preview_token(&slots, &sections) != token {
        return Err(CollectionItemError::StaleRenumber);
    }
    let plan = plan_renumber(&slots, &sections, locked.number_width)?;
    write_layout(&mut tx, collection_id, Kind::Standing, &plan.order).await?;
    for change in plan.section_changes.iter().filter(|c| c.from != c.to) {
        sqlx::query!(
            r#"UPDATE collection_section SET starts_at = $2, updated_at = now()
               WHERE id = $1 AND collection_id = $3"#,
            change.section_id,
            change.to,
            collection_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    commit(tx).await?;
    Ok(RenumberOutcome {
        changes: plan.changes,
        section_changes: plan.section_changes,
    })
}

/// Labels 1..n in the current order, inside an open transaction that holds
/// the collection lock. Shared by [`renumber`] and a standing → program type
/// change.
async fn close_gaps(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    slots: Vec<Slot>,
) -> Result<(), CollectionItemError> {
    let order: Vec<Slot> = slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| Slot {
            id: slot.id,
            label: Label::plain(u16::try_from(index + 1).unwrap_or(MAX_LABEL_NUMBER)),
        })
        .collect();
    write_layout(tx, collection_id, Kind::Standing, &order).await
}

/// For a type or width change of a collection whose row the caller has
/// locked: the highest label number in use (0 when empty).
pub(crate) async fn highest_number_in(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
) -> Result<u16, sqlx::Error> {
    Ok(live_slots(tx, collection_id)
        .await?
        .iter()
        .map(|s| s.label.number())
        .max()
        .unwrap_or(0))
}

/// For a standing → program type change, under the caller's collection
/// lock: close the gaps, so the labels become the positions.
pub(crate) async fn close_gaps_for_program(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
) -> Result<(), sqlx::Error> {
    let slots = live_slots(tx, collection_id).await?;
    close_gaps(tx, collection_id, slots)
        .await
        .map_err(|err| match err {
            CollectionItemError::Database(err) => err,
            // Rewriting positions and labels touches neither the
            // arrangement uniqueness nor a foreign key, so only the database
            // itself can fail here.
            other => sqlx::Error::Protocol(other.to_string()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(text: &str) -> Label {
        Label::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    fn numbering(labels: &[&str]) -> Numbering {
        Numbering::new(labels.iter().map(|t| l(t)), 3)
    }

    #[test]
    fn labels_parse_case_insensitively_and_strictly() {
        assert_eq!(l("12A"), l("12a"));
        assert_eq!(l(" 7 ").to_string(), "7");
        for bad in ["", "0", "012", "12ab", "a", "12-a", "10000", "1.5", "-3"] {
            assert_eq!(Label::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Label::parse_padded("007"), Some(l("7")));
        assert_eq!(Label::parse_padded("012a"), Some(l("12a")));
        assert_eq!(Label::parse_padded("000"), None);
    }

    #[test]
    fn labels_order_naturally() {
        let mut labels = [l("13"), l("12b"), l("2"), l("12"), l("12a"), l("100")];
        labels.sort();
        let text: Vec<String> = labels.iter().map(Label::to_string).collect();
        assert_eq!(text, ["2", "12", "12a", "12b", "13", "100"]);
    }

    #[test]
    fn padded_folder_names_sort_like_the_labels() {
        assert_eq!(l("7").padded(3), "007");
        assert_eq!(l("12a").padded(3), "012a");
        assert_eq!(l("7").padded(4), "0007");
        // `-` sorts before letters, so 012- < 012a- < 013-.
        let mut names = vec![
            folder_name("13", 3, "c"),
            folder_name("12a", 3, "b"),
            folder_name("12", 3, "a"),
        ];
        names.sort();
        assert_eq!(names, ["012-a", "012a-b", "013-c"]);
    }

    #[test]
    fn allowed_labels_are_gaps_the_next_number_or_a_letter() {
        let n = numbering(&["1", "2", "5", "9"]);
        assert_eq!(n.check(l("3")), Ok(()), "a gap");
        assert_eq!(n.check(l("10")), Ok(()), "the next number");
        assert_eq!(
            n.check(l("11")),
            Err(LabelRefusal::NotAllowed),
            "past the end"
        );
        assert_eq!(n.check(l("5")), Err(LabelRefusal::Taken));
        assert_eq!(n.check(l("5a")), Ok(()), "a letter after a used number");
        assert_eq!(n.check(l("5b")), Ok(()), "5b needs no 5a");
        assert_eq!(
            n.check(l("4a")),
            Err(LabelRefusal::NotAllowed),
            "4 is unused"
        );
        assert_eq!(n.check(l("1000")), Err(LabelRefusal::TooWide));
    }

    #[test]
    fn an_empty_collection_starts_at_one() {
        let n = numbering(&[]);
        assert_eq!(n.check(l("1")), Ok(()));
        assert_eq!(n.check(l("2")), Err(LabelRefusal::NotAllowed));
        assert_eq!(n.check(l("1a")), Err(LabelRefusal::NotAllowed));
        assert_eq!(
            n.choices(),
            LabelChoices {
                suggested: Some(l("1")),
                gaps: vec![],
                sections: vec![],
            }
        );
    }

    #[test]
    fn choices_list_the_gaps_and_the_next_number() {
        let n = numbering(&["1", "2", "5", "5a", "9"]);
        assert_eq!(
            n.choices(),
            LabelChoices {
                suggested: Some(l("10")),
                gaps: vec![l("3"), l("4"), l("6"), l("7"), l("8")],
                sections: vec![],
            }
        );
        let full = numbering(&["999"]);
        assert_eq!(full.choices().suggested, None);
    }

    #[test]
    fn a_moved_piece_does_not_count_against_itself() {
        let n = numbering(&["1", "2", "3"]);
        // Moving 3 to 4 would invent a gap at 3: refused, as removing 3 and
        // adding it back would allow only 3.
        assert_eq!(
            n.without(&l("3")).check(l("4")),
            Err(LabelRefusal::NotAllowed)
        );
        assert_eq!(n.without(&l("2")).check(l("2")), Ok(()));
    }

    fn section(name: &str, starts_at: u16) -> SectionBound {
        SectionBound {
            // Deterministic ids, so plans compare across calls.
            id: Uuid::from_u128(u128::from(starts_at)),
            name: name.to_string(),
            starts_at,
        }
    }

    /// Märsche 1–3 (no section heading before 61), Polkas from 61 holding
    /// 61–63, and the given sections on top.
    fn folder(extra: &[SectionBound]) -> Numbering {
        let mut sections = vec![section("Polkas", 61)];
        sections.extend_from_slice(extra);
        numbering(&["1", "2", "3", "61", "63"]).with_sections(sections)
    }

    #[test]
    fn a_new_section_past_the_end_makes_its_start_allowed() {
        let without = folder(&[]);
        assert_eq!(
            without.check(l("101")),
            Err(LabelRefusal::NotAllowed),
            "past the end without a section: a typo"
        );
        let n = folder(&[section("Modern", 101)]);
        assert_eq!(n.check(l("101")), Ok(()), "the empty section's start");
        assert_eq!(
            n.check(l("102")),
            Err(LabelRefusal::NotAllowed),
            "an empty section offers its start only"
        );
        assert_eq!(n.check(l("64")), Ok(()), "the next polka");
        assert_eq!(n.check(l("62")), Ok(()), "a gap among the polkas");
        assert_eq!(
            n.check(l("80")),
            Err(LabelRefusal::NotAllowed),
            "below the highest overall, but far past the polkas"
        );
        assert_eq!(n.check(l("4")), Ok(()), "the next march");
        assert_eq!(n.check(l("5")), Err(LabelRefusal::NotAllowed));
    }

    #[test]
    fn a_section_can_divide_the_numbers_in_use() {
        // "Polkas" now starts at 3, inside the marches: 3 falls into it.
        let n = numbering(&["1", "2", "3", "4"]).with_sections([section("Polkas", 3)]);
        assert_eq!(n.check(l("5")), Ok(()), "the next polka");
        assert_eq!(n.check(l("3")), Err(LabelRefusal::Taken));
        assert_eq!(
            n.check(l("6")),
            Err(LabelRefusal::NotAllowed),
            "still no number past the end"
        );
        // 3 is the next march number, but it is the polkas' start and taken.
        let choices = n.choices();
        assert_eq!(choices.sections.len(), 2);
        assert_eq!(choices.sections[0].section, None);
        assert_eq!(choices.sections[0].next, None, "the marches are full");
        assert_eq!(choices.sections[1].next, Some(l("5")));
    }

    #[test]
    fn a_full_section_offers_no_next_number() {
        let n = numbering(&["1", "2", "60", "61"])
            .with_sections([section("Märsche", 1), section("Polkas", 61)]);
        assert_eq!(n.check(l("61")), Err(LabelRefusal::Taken));
        assert_eq!(n.check(l("60a")), Ok(()), "a letter label still fits");
        assert_eq!(n.check(l("3")), Ok(()), "a gap still fits");
        let choices = n.choices();
        assert_eq!(
            choices.sections,
            vec![
                SectionChoice {
                    section: Some(section("Märsche", 1)),
                    starts_at: 1,
                    next: None,
                },
                SectionChoice {
                    section: Some(section("Polkas", 61)),
                    starts_at: 61,
                    next: Some(l("62")),
                },
            ],
            "no entry before a section starting at 1"
        );
        assert_eq!(choices.gaps.first(), Some(&l("3")));
        assert_eq!(choices.gaps.len(), 57, "3..=59");
        assert_eq!(choices.suggested, Some(l("62")));
    }

    #[test]
    fn choices_list_each_sections_next_number() {
        let n = folder(&[section("Modern", 101)]);
        let choices = n.choices();
        assert_eq!(choices.suggested, Some(l("64")));
        assert_eq!(choices.gaps, vec![l("62")]);
        let next: Vec<(u16, Option<Label>)> = choices
            .sections
            .iter()
            .map(|c| (c.starts_at, c.next))
            .collect();
        assert_eq!(
            next,
            vec![
                (1, Some(l("4"))),
                (61, Some(l("64"))),
                (101, Some(l("101")))
            ]
        );
    }

    fn slots(labels: &[&str]) -> Vec<Slot> {
        labels
            .iter()
            .enumerate()
            .map(|(i, t)| Slot {
                id: Uuid::from_u128(1000 + i as u128),
                label: l(t),
            })
            .collect()
    }

    fn plan_labels(plan: &RenumberPlan) -> Vec<String> {
        plan.changes.iter().map(|c| c.to.clone()).collect()
    }

    fn plan_starts(plan: &RenumberPlan) -> Vec<(i32, i32)> {
        plan.section_changes
            .iter()
            .map(|c| (c.from, c.to))
            .collect()
    }

    #[test]
    fn renumber_moves_section_starts_to_their_first_piece() {
        let plan = plan_renumber(
            &slots(&["1", "1a", "3", "61", "61a", "70"]),
            &[section("Polkas", 61)],
            3,
        )
        .unwrap();
        assert_eq!(plan_labels(&plan), ["1", "2", "3", "4", "5", "6"]);
        assert_eq!(plan_starts(&plan), [(61, 4)]);
    }

    #[test]
    fn renumber_keeps_one_number_for_each_empty_section() {
        // Märsche 1–3, an empty "Modern" from 50 between them and the
        // polkas, the polkas from 61, and two empty sections at the end.
        let plan = plan_renumber(
            &slots(&["1", "2", "3", "61", "62"]),
            &[
                section("Modern", 50),
                section("Polkas", 61),
                section("Choräle", 200),
                section("Hymnen", 300),
            ],
            3,
        )
        .unwrap();
        assert_eq!(
            plan_labels(&plan),
            ["1", "2", "3", "5", "6"],
            "4 stays free for the empty section between"
        );
        assert_eq!(
            plan_starts(&plan),
            [(50, 4), (61, 5), (200, 7), (300, 8)],
            "starts stay in order and distinct"
        );
    }

    #[test]
    fn renumber_without_sections_is_one_to_n() {
        let plan = plan_renumber(&slots(&["2", "5", "5a"]), &[], 3).unwrap();
        assert_eq!(plan_labels(&plan), ["1", "2", "3"]);
        assert!(plan.section_changes.is_empty());
    }
}
