# Lied — Sheet Music Management Tool

## Personas

### 1. The Working Musician

A professional or semi-professional musician with a complex, multi-context musical life:

- **Orchestral:** Has one home orchestra (primary base), occasionally plays as a substitute/aide in other orchestras.
- **Ensembles:** Plays in smaller chamber groups with frequently changing partners.
- **Solo practice:** Regular home practice sessions.
- **Performance contexts:** Formal concerts, and *Gebrauchsmusik* (functional music supporting events, ceremonies, parties, etc.).
- **Instruments:** May play two related instruments (e.g. violin + viola, trumpet + flugelhorn), with potentially different parts for each.
- **Sheet music sources:** Many — orchestras, publishers, personal arrangements, scanned/photocopied parts.
- **Annotations:** Has personalized scores with bowings, fingerings, breath marks, cues, and other personal markings.
- **Needs:** Quick access to the right part for the right instrument in the right context; preserving personal annotations across versions; handling music that is customized to their specific needs.

**Sub-role: Principal (Satzführer)**
A section leader who is also a Working Musician but carries additional coordination responsibilities:
- Ensures every musician in their section has the correct edition, version, and voice for a given piece.
- Before a concert or event, verifies that all voices in the section are covered and adequately rehearsed.
- The personnel/scheduling aspect (who plays what seat) is out of scope, but **voice/part distribution within a section** is a relevant application concern — knowing which voices exist in a piece and confirming they are assigned.

### 2. The Orchestra Archivist

A dedicated role within an orchestra, often part of a small team with a head archivist. Operates at the intersection of library management, logistics, and coordination with artistic leadership.

**Music sources:**
- Primarily purchased arrangements from publishers (licensed copies).
- Occasionally internally arranged pieces by orchestra members.

**Archive management:**
- Maintains a master archive of all arrangements the orchestra owns — organized, searchable, and accessible to the conductor and artistic staff.
- Tracks provenance (publisher, license, arranger), instrumentation, and physical/digital copies.

**Collections:**
- Manages multiple named, curated collections active at any given time. All collections are indexed by piece number (local to the collection):
  - **Concert programs:** Each upcoming concert has a program; the archivist assembles and distributes the relevant parts to each musician. The sequence is fixed and pre-planned.
  - **Standing repertoire:** A go-to collection for recurring informal events (e.g. Oktoberfest-style gatherings, civic functions). Indexed but drawn from flexibly during performance.
  - **Indexed books:** For marching bands, a numbered march book; for jazz ensembles, a standard repertoire book — pieces referenced by their index number.
- Collections are updated in consultation with the conductor or artistic director.

**Distribution:**
- Sends or syncs individual parts to musicians — the right part to the right person for the right event.
- Must handle per-musician instrument assignments (who plays what in this ensemble for this program).

**Needs:** A reliable, searchable archive; easy assembly of per-concert part sets; smooth distribution/sync to musicians; clear collection versioning so everyone has the current program.

### 3. The Conductor / Artistic Director

Often the same person, but the roles are separable. Bridges artistic vision and practical repertoire management.

**Program planning:**
- Searches the archive by theme, mood, instrumentation, difficulty, duration, or other metadata to build concert programs.
- Also searches external sources (publishers, online catalogs, recordings) for new repertoire, then initiates purchase and archiving.
- Is consulted on which variant or edition of a piece to acquire when multiple options exist.

**Rehearsal:**
- Works through the score with the ensemble during practice, trying out interpretive options.
- Needs to add *global annotations* — markings that apply to all parts or the full ensemble (e.g. tempo changes, dynamic shaping, structural cuts) — as opposed to a musician's personal annotations on their individual part.

**Performance:**
- Either follows a fixed, pre-planned program (concert mode: piece by piece, in order) or selects by piece number from a standing collection (e.g. "pieces we can play at short notice for this type of event").
- Needs a clear, distraction-free view of what's on the program and what's available.

**Needs:** Powerful archive search with rich metadata; a smooth pipeline from "found something online" to "in our archive"; global annotation support distinct from per-musician markings; flexible program execution (fixed sequence vs. selection by piece number from a known collection).

---

## Supported Formats

| Format | Role | Description |
|---|---|---|
| **MusicXML** | Interchange | Most notation software can export/import it |
| **LilyPond** | Source | Human-readable, version-control friendly, renders to PDF |
| **PDF** | Display | Universal display/print format; not editable |
| **Image** (PNG, JPG, etc.) | Display | Scanned or photographed scores; display only |

**Conversion hierarchy** (transparency about what is lossless vs. lossy is a design requirement):

- **Easy / lossless or near-lossless:**
  - LilyPond → PDF (native)
  - MusicXML → LilyPond (via `musicxml2ly`)
  - MusicXML → PDF (via LilyPond or MuseScore pipeline)
  - PDF → image (rendering only)
- **Hard / approximate (OMR — Optical Music Recognition):**
  - PDF or image → MusicXML or LilyPond (imperfect; requires OMR tooling, results need human review)

The application should surface the format of each stored file and make conversion options available where applicable, clearly indicating whether a conversion is clean or approximate.

---

## Architecture

### Guiding principles
- **Self-hostability is a hard requirement.** No mandatory cloud dependencies.
- **Open protocols over proprietary ones.** Music should be accessible without this app installed.
- **Defer the display app.** Existing tools (PDF readers, MuseScore, Frescobaldi, forScore, MobileSheets) cover display well enough to validate workflows first. Build a custom display client only once we know what it needs to do better than existing tools.

### Backend stack

```
PostgreSQL          ← structured metadata, collections, programs, annotations, users, roles
MinIO               ← file storage (S3-compatible, single binary, self-hostable)
    │
    ├── WebDAV interface   ← open access: OS mounting, tablet file managers, MuseScore,
    │                         Frescobaldi, any WebDAV-capable app — no Lied app required
    └── REST API           ← music-aware operations: search, collections, part assignment,
                              annotations, format conversion pipeline
```

### Why this split
- WebDAV and REST are complementary, not competing. WebDAV is a filesystem view of the files; REST is the music-aware layer on top. Both can be served from the same backend simultaneously.
- WebDAV gives musicians immediate access via OS-native mounting or any compatible app, with no lock-in to this project.
- REST enables the features WebDAV has no concept of: metadata search, collection management, part distribution, annotation storage, conversion triggering.
- MinIO is S3-compatible, meaning every language ecosystem has mature client libraries for it, and it can be swapped for any S3-compatible service without changing application code.

### Auth & access

**Authentication:**
- Local accounts (username/password, argon2id hashing) are the baseline — required for self-hostable deployments without external dependencies.
- OIDC is an optional, configurable second auth method for orgs that want SSO.
- WebDAV uses per-user **app passwords** (long random tokens, revocable, WebDAV-scoped). App passwords are cached on devices and must not share a session with the REST/web login.
- REST API uses session cookies (web client) or bearer tokens (programmatic).

**Authorization:**
- Permissions are scoped to organizations. A user's capabilities within an org are determined by `Membership.role`.
- No system-wide application roles. An optional `User.is_system_admin` boolean covers instance-level operations (org provisioning, user management).
- A `PartAssignment` implicitly grants the assigned user **read access to that voice's files** for as long as the assignment exists, regardless of Membership in the org. This is how guests and substitutes access parts.

### WebDAV layout

Canonical directory tree (also the public contract for external tools mounting the volume):

```
/orgs/<org-slug>/
    arrangements/
        <arrangement-id>-<title-slug>/
            score/                          ← full-score files
                <name>.{pdf,musicxml,ly}
            voices/
                <voice-slug>/
                    <name>.{pdf,musicxml,ly}   ← canonical voice files
                    annotations/
                        <user-slug>/
                            <name>.pdf         ← personal annotation files
    collections/
        <collection-slug>/                  ← read-only computed view; paths assembled
            <index>-<arrangement-slug>/        from CollectionItems + PartAssignments
                ...

/users/<user-slug>/
    library/                                ← personal/private files (solo practice)
```

- Soft-deleted rows (`deleted_at != NULL`) are hidden from WebDAV listings.
- The `/orgs/<org>/collections/` subtree is a read-only computed view. Writes go through `/orgs/<org>/arrangements/`.
- `/users/<user>/library/` is private to that user, not bound to any org.

### Display clients (deferred)
Musicians can use existing apps against the WebDAV interface:
- **PDF readers on tablet:** forScore, MobileSheets, Xodo (performance use)
- **Notation software:** MuseScore (MusicXML), Frescobaldi (LilyPond)
- **OS file manager:** for general browsing and access

A purpose-built display/reader app is a future consideration, not an initial requirement.

---

## Non-functional posture

For the first few iterations the operational target is "as fast and cheap as possible." Hard NFR numbers (latency budgets, availability targets, scale ceilings) are not pinned yet — they get pinned as use grows. The constraint on the design is that none of the choices above should foreclose future scaling, multi-tenancy, or hosted operation.

### Operational stance
- **Target deployment:** single self-hosted instance, small numbers of orgs and users. Hosted multi-tenant operation is a future scenario the design must not preclude.
- **Configurable limits.** Whenever a limit is imposed (max upload size, rate-limit window, per-org storage cap, conversion job timeout, etc.) it MUST be documented and configurable. Hardcoded limits are not acceptable.

### Security baseline
- **Rate limiting** is built in from day 1, with per-route and per-identity buckets configurable per deployment.
- **Audit logging** is shallow but present from day 1 — log who did what to which entity (auth events, CRUD on Arrangement/Voice/File/Collection, role changes, hard deletes). Depth grows as needs become specific.
- **Secrets management:**
  - All secrets are read from environment variables. Every secret env var `X` also accepts `X_FILE` pointing at a path — the Docker secrets convention, which works with Docker Compose/Swarm secrets, Kubernetes secrets, and systemd `LoadCredential=`.
  - The config layer abstracts secret sources so each secret can declare `env:`, `file:`, or `cmd:` (shells out to a helper like `pass`, `op read`, `vault kv get`). Vault / Infisical / 1Password / Bitwarden plug in later without coupling the app to any of them.
  - Secret values are redacted from logs and error responses at the config-layer boundary, not at every call site.
  - Session/JWT signing keys support hot rotation via an admin REST endpoint (new key written, old key honored for a grace window). DB/MinIO credentials tolerate restart for rotation.
  - **Anti-patterns:** no mandatory Vault dependency, no secrets committed to the repo (provide `.env.example` only), no custom encrypted-secrets-file format.

---

## Use Cases

### Archive & Catalog
1. Add an arrangement to the archive (from publisher purchase or internal arrangement)
2. Search the archive by metadata (theme, instrumentation, duration, difficulty, composer, etc.)
3. Track provenance and licensing per arrangement
4. Discover external repertoire and initiate a purchase → archive pipeline

### Collections & Programs
5. Create and maintain a concert program (fixed sequence, indexed by piece number)
6. Create and maintain a standing collection (indexed by piece number, drawn from flexibly — Gebrauchsmusik, march book, jazz standards book)
7. Update a collection after consultation with conductor/artistic director

### Format Management & Conversion
8. Store a piece in one or more formats; treat them as representations of the same work
9. Convert between formats where feasible (e.g. LilyPond → PDF, MusicXML → LilyPond)
10. Surface conversion quality transparently (clean vs. OMR-approximate)
11. Accept scanned/image scores as a valid (display-only) format without forcing conversion

### Part Distribution & Access
12. Distribute or sync the correct part to each musician for a given program or event
13. Verify all voices in a section are covered (principal role)
14. Access my parts across multiple contexts (home orchestra, guest ensemble, solo practice)

### Annotations
15. Add and preserve personal annotations on a part (musician-level; survives format updates)
16. Add global annotations during rehearsal (conductor-level; applies across all parts of a piece)

### Performance
17. Perform from a fixed program in sequence (concert mode)
18. Select by piece number from a standing collection during performance (Gebrauchsmusik / ad-hoc mode)

### Multi-instrument & Multi-version
19. Manage parts for two instruments under one musician identity
20. Handle multiple editions or versions of the same piece; track which is in active use

---

## Data Model

### Decisions
- **Provenance** is flat metadata on `Arrangement` (publisher, arranger, purchase date, license notes, copy count). No separate publisher/license entity unless querying by publisher becomes a clear need.
- **Editions/variants (UC-19, UC-20).** Each `Arrangement` is already a specific edition. Multiple editions of one work = multiple `Arrangement` rows sharing a `Work`. `Arrangement.status` (`active`, `archived`) lets an org retire an old edition without deleting it; multiple editions may be active simultaneously.
- **DB is source of truth for existence; MinIO versioning is content history only.** Setting `deleted_at` hides a row from REST, WebDAV, and search; the MinIO object remains. Undelete clears `deleted_at` — no MinIO operation. Hard delete (admin-only) removes both the DB row and all MinIO object versions. MinIO version history is reachable through REST admin endpoints but never surfaced via WebDAV.
- **Soft delete** via `deleted_at` on Arrangement, Voice, File, Collection, CollectionItem.
- **Conversion provenance.** `File.derived_from_file_id` (nullable FK) links a derived file to its source; a file with no `derived_from` is a source. `File.conversion_quality` (`clean`, `omr`, `manual`) surfaces UC-10's lossless-vs-approximate distinction. No separate `Conversion` table — the FK + quality enum is sufficient until we need failure history.
- **Image subtype.** `File.mime_type` (text) records the actual subtype (`image/png`, `image/jpeg`, `image/tiff`, …). The `format` enum stays coarse and drives routing (e.g. `image` → display-only).
- **Audit fields.** `created_at`, `updated_at`, `created_by` (user FK, nullable for system) on Arrangement, Voice, File, Collection, CollectionItem, GlobalAnnotation.
- **Distribution (UC-12) and coverage (UC-13).** No separate distribution table; `PartAssignment.notified_at` and `PartAssignment.acknowledged_at` (both nullable timestamps) carry the state. Coverage check: every required voice has a `PartAssignment`; "rehearsed" coverage additionally requires `acknowledged_at`.
- **Personal annotations** are files in MinIO, no DB record. Naming convention: `orgs/<org>/arrangements/<arr>/voices/<voice>/annotations/<user-slug>/<name>.pdf` — alongside the *voice*, not a specific source-file format. Annotations are independent of source-file updates (this is what "survives format updates" in UC-15 means). The app surfaces a staleness indicator by comparing the annotation's mtime to the source file's latest MinIO version mtime.
- **Global annotations** are structured records in PostgreSQL — queryable, shared, authored by conductor/director.
- **Search backend.** Postgres FTS (`tsvector` + GIN) over title, composer, arranger, instrumentation description, and tag names; `pg_trgm` for fuzzy matching on title and composer. No external search service — preserves the single-Postgres self-hostable footprint. If volume ever outgrows this, swap is mechanical.
- **Structured search fields.** Conductor's UC-2 dimensions split into structured columns vs. tags:
  - `Work.composer` (text, nullable) — composer lives on the abstract work, not the edition. Arrangements without a Work have no composer (folk, internal compositions).
  - `Arrangement.duration_seconds` (int, nullable) — range queries.
  - `Arrangement.difficulty` (smallint 1–8, nullable) — **ABRSM scale** is the canonical numeric for filters and ranges.
  - `Arrangement.difficulty_ratings` (jsonb, nullable) — optional map of scale → grade for cross-referencing, e.g. `{"abrsm": "6", "aba": "3.5", "henle": "5", "rcm": "8"}`. The `abrsm` key, if present, must agree with `difficulty` (rounded). UI suggests known scales (ABRSM, ABA, Henle, RCM, Trinity) via autocomplete.
  - `Arrangement.difficulty_notes` (text, nullable) — prose caveats (e.g. "grade 5 except the cadenza"); expected to be rarely used.
  - Instrumentation filter joins through `Voice.instrument`; no new field.
- **Tags.** Open-ended dimensions (theme, mood, era, occasion, style) live in a per-org `Tag` entity with a free-text `kind` and a many-to-many `ArrangementTag` join. Per-org so vocabularies don't leak between orgs; `kind` is text (not enum) so orgs can invent categories without a migration.

### Entities

**Work** *(optional)*
The abstract musical work (e.g. "Beethoven: Symphony No. 5"). Lightweight, used for grouping and search. Not every arrangement requires one.
Fields: title, `composer` (text, nullable).

**Arrangement**
A specific arrangement for specific instrumentation — what an organization licenses and owns. Also the unit of edition/variant: multiple editions of one work are multiple `Arrangement` rows under the same `Work`.
Fields: title, work (optional FK), instrumentation description, arranger, publisher, purchase date, license notes, copy count allowed, status (enum: `active`, `archived`), `duration_seconds` (int, nullable), `difficulty` (smallint 1–8, ABRSM, nullable), `difficulty_ratings` (jsonb, nullable), `difficulty_notes` (text, nullable), audit fields, `deleted_at`.

**Voice**
An individual instrument part within an arrangement (e.g. Flute 1, Violin II, Trumpet in Bb).
Fields: arrangement FK, name, instrument, audit fields, `deleted_at`.

**File**
An actual file representing a voice or full score, in a specific format.
Fields: voice FK (nullable for full scores), arrangement FK, format (enum: `lilypond`, `musicxml`, `pdf`, `image`), mime_type, storage path (MinIO), `derived_from_file_id` (nullable FK to File), `conversion_quality` (enum: `clean`, `omr`, `manual`; null if not derived), audit fields, `deleted_at`.

**Organization**
An orchestra or ensemble.

**User**
A person using the system. No system-wide application role — authorization is per-org via `Membership.role`. Optional `is_system_admin` boolean for instance-level operations (org provisioning, user management).

**Membership**
A user's membership in an organization.
Fields: user FK, organization FK, role, instruments (array), `is_principal` (bool), `principal_instruments` (array, subset of the instruments this member leads — array because principals may cover doublings, e.g. trumpet + flugelhorn).
Coverage check (UC-13): for an arrangement in a collection, every `Voice` whose `instrument ∈ principal_instruments` must have a `PartAssignment`.

**Collection**
A named, indexed set of arrangements belonging to an organization.
Fields: organization FK, name, type (enum: `program`, `standing`), audit fields, `deleted_at`.
All collections are indexed by piece number local to the collection. `program` collections have a fixed sequence; `standing` collections are drawn from by number during performance.

**CollectionItem**
An arrangement within a collection with its local index number.
Fields: collection FK, arrangement FK, index number, audit fields, `deleted_at`.

**PartAssignment**
Which user plays which voice for a specific item in a collection.
Fields: collection item FK, user FK, voice FK, `notified_at` (nullable timestamp), `acknowledged_at` (nullable timestamp).
A PartAssignment does **not** require the user to hold a Membership in the org — this is how guest and substitute musicians are modeled. An assignment implicitly grants the assigned user read access to that voice's files for the lifetime of the assignment. `notified_at`/`acknowledged_at` back UC-12 distribution and UC-13 coverage state without a separate distribution table.

**GlobalAnnotation**
A conductor- or director-level annotation on an arrangement, visible to all.
Fields: arrangement FK, author (user FK), type, content, audit fields.

**Tag**
An open-ended classifier scoped to an organization, used for theme/mood/era/occasion/style and similar dimensions the conductor searches by.
Fields: organization FK, name, kind (text; suggested values: `theme`, `mood`, `era`, `occasion`, `style`, `other`), audit fields, `deleted_at`.
Unique on `(organization, name, kind)`.

**ArrangementTag**
Many-to-many join between Arrangement and Tag.
Fields: arrangement FK, tag FK.

### Structure

```
Work (optional, composer)
  └── Arrangement (status, duration, difficulty 1-8 ABRSM, difficulty_ratings, provenance)
        ├── File(s) [full score]
        ├── ArrangementTag → Tag
        └── Voice
              └── File (format, mime_type, derived_from?, conversion_quality?)
                    └── [personal annotation files in MinIO, by naming convention]

GlobalAnnotation → Arrangement

Organization
  ├── Membership → User (role, instruments)
  ├── Tag (name, kind)
  └── Collection (program | standing, indexed by piece number)
        └── CollectionItem (index number) → Arrangement
              └── PartAssignment → User + Voice (notified_at, acknowledged_at)
```

---

## Phase 1 cut

Smallest version that closes the **archivist → musician loop** end-to-end: an archivist uploads a piece, builds a concert program, assigns parts; each musician opens their part on an iPad via WebDAV. Later phases plug in cleanly because the data model already accommodates them.

### In scope for phase 1

**Backend infrastructure**
- Postgres + MinIO + REST API + WebDAV server.
- Local accounts only (argon2id); session cookies + bearer tokens for REST; app passwords for WebDAV.
- Rate limiting, shallow audit log, env / `_FILE` / `cmd:` secrets layer.

**Entities** — full data model from the spec: Org, User, Membership, Work, Arrangement, Voice, File, Collection, CollectionItem, PartAssignment, Tag, ArrangementTag, GlobalAnnotation. Even entities whose flows are deferred get their tables so the schema is stable.

**File handling**
- Upload/download via REST.
- Files stored and served as-is — **no conversion, no OMR**.
- All four formats (lilypond/musicxml/pdf/image) accepted; `format` and `mime_type` recorded but no pipeline runs on them.

**WebDAV**
- `/orgs/<org>/arrangements/...` — read-write for archivists, read for assigned musicians.
- `/orgs/<org>/collections/...` — read-only computed view.
- `/users/<user>/library/` — private personal area.

**Search** — basic listing + ILIKE filter on title/composer. No FTS, no `pg_trgm`, no tag-based search yet.

**Annotations** — personal annotations work by file convention via WebDAV (musicians drop PDFs in `…/annotations/<user-slug>/`). No staleness indicator. No global annotations flow.

**UI** — minimal admin web UI for the archivist (entity CRUD, file upload, build collections, assign parts). Conductor uses the same UI for program planning. Musicians have no UI — WebDAV is their interface.

### Deferred to phase 2+

Model supports these; phase 1 has no implementation.

- OIDC auth.
- Search: FTS, `pg_trgm`, tag-based search, `difficulty_ratings` cross-scale lookups, faceted filters.
- Conversion pipelines (LilyPond → PDF, MusicXML → LilyPond, etc.) and the conversion-quality UI surface.
- OMR (Audiveris integration).
- Global annotations workflow.
- Distribution flow: notification email, acknowledgement UI (`notified_at` / `acknowledged_at` fields exist but unused).
- Principal coverage check (UC-13).
- Performance UI (UC-17, UC-18).
- External repertoire discovery (UC-4).
- Edition-switching UX (UC-20) and per-instrument variants per Membership (UC-19) — model supports both; UI/flow deferred.
- Annotation staleness indicator.
- Purpose-built display app (already deferred indefinitely in the spec).

### Trade-off accepted

Phase 1 is unglamorous from the conductor's perspective (title ILIKE only, no tags, no fancy program planning aids) and from the format-conversion angle (you upload what you upload). It delivers the actual base value — getting parts from archivist to musician — and every later phase plugs in cleanly because the data model already accommodates it.
