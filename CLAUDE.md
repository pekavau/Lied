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

| Format | Role |
|---|---|
| **MusicXML** | Interchange standard; most notation software can export/import it |
| **LilyPond** | Source format; human-readable, version-control friendly, renders to PDF |
| **PDF** | Universal display/print format; not editable |
| **Image** (PNG, JPG, etc.) | Scanned or photographed scores; display only |

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

### Display clients (deferred)
Musicians can use existing apps against the WebDAV interface:
- **PDF readers on tablet:** forScore, MobileSheets, Xodo (performance use)
- **Notation software:** MuseScore (MusicXML), Frescobaldi (LilyPond)
- **OS file manager:** for general browsing and access

A purpose-built display/reader app is a future consideration, not an initial requirement.

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
- **File versioning** is handled at the storage layer (MinIO object versioning enabled on the bucket). No application-level version model.
- **Soft delete** via `deleted_at` on Arrangement, Voice, File, Collection, CollectionItem — accident recovery without a version history UI.
- **Personal annotations** are files in MinIO stored by naming convention alongside the source file. No database record — a musician's annotated PDF is just another file, independent of the source.
- **Global annotations** are structured records in PostgreSQL — queryable, shared, authored by conductor/director.

### Entities

**Work** *(optional)*
The abstract musical work (e.g. "Beethoven: Symphony No. 5"). Lightweight, used for grouping and search. Not every arrangement requires one.

**Arrangement**
A specific arrangement for specific instrumentation — what an organization licenses and owns.
Fields: title, work (optional FK), instrumentation description, arranger, publisher, purchase date, license notes, copy count allowed, `deleted_at`.

**Voice**
An individual instrument part within an arrangement (e.g. Flute 1, Violin II, Trumpet in Bb).
Fields: arrangement FK, name, instrument, `deleted_at`.

**File**
An actual file representing a voice or full score, in a specific format.
Fields: voice FK (nullable for full scores), arrangement FK, format (enum: `lilypond`, `musicxml`, `pdf`, `image`), storage path (MinIO), canonical flag, `deleted_at`.

**Organization**
An orchestra or ensemble.

**User**
A person using the system. System-wide roles: musician, archivist, conductor.

**Membership**
A user's membership in an organization.
Fields: user FK, organization FK, role, instruments (array).

**Collection**
A named, indexed set of arrangements belonging to an organization.
Fields: organization FK, name, type (enum: `program`, `standing`), `deleted_at`.
All collections are indexed by piece number local to the collection. `program` collections have a fixed sequence; `standing` collections are drawn from by number during performance.

**CollectionItem**
An arrangement within a collection with its local index number.
Fields: collection FK, arrangement FK, index number, `deleted_at`.

**PartAssignment**
Which user plays which voice for a specific item in a collection.
Fields: collection item FK, user FK, voice FK.

**GlobalAnnotation**
A conductor- or director-level annotation on an arrangement, visible to all.
Fields: arrangement FK, author (user FK), type, content, timestamp.

### Structure

```
Work (optional)
  └── Arrangement (flat provenance fields)
        ├── File(s) [full score]
        └── Voice
              └── File (format, canonical flag, deleted_at)
                    └── [personal annotation files in MinIO, by naming convention]

GlobalAnnotation → Arrangement

Organization
  ├── Membership → User (role, instruments)
  └── Collection (program | standing, indexed by piece number)
        └── CollectionItem (index number) → Arrangement
              └── PartAssignment → User + Voice
```
