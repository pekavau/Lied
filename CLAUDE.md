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
Lied app (Rust + axum)         ← serves both interfaces
    ├── WebDAV interface       ← open access: OS mounting, tablet file managers,
    │                            MuseScore, Frescobaldi, any WebDAV-capable app
    └── REST API + admin UI    ← music-aware operations: search, collections,
                                 part assignment, annotations, conversion pipeline
        │
        ├── PostgreSQL         ← structured metadata: orgs, users, memberships,
        │                        arrangements, collections, annotations, tags, audit log
        └── MinIO              ← file storage (S3-compatible, self-hostable)
```

### Why this split
- WebDAV and REST are complementary, not competing. WebDAV is a filesystem view of the files; REST is the music-aware layer on top. Both can be served from the same backend simultaneously.
- WebDAV gives musicians immediate access via OS-native mounting or any compatible app, with no lock-in to this project.
- REST enables the features WebDAV has no concept of: metadata search, collection management, part distribution, annotation storage, conversion triggering.
- MinIO is S3-compatible, meaning every language ecosystem has mature client libraries for it, and it can be swapped for any S3-compatible service without changing application code.

### Auth & access

**Authentication:**
- Local accounts (username/password, argon2id hashing) are the baseline — required for self-hostable deployments without external dependencies.
- OIDC is an optional, configurable second auth method for orgs that want SSO. When OIDC is configured, **local login stays available as a fallback by default** — the break-glass path if the IdP is misconfigured or down. Account linking is by matching verified `email`. A per-org "disable local auth" (SSO-only) policy is a phase-2 toggle; phase 1 keeps both paths to avoid lockout on self-hosted instances.
- WebDAV uses per-user **app passwords** (long random tokens, revocable, WebDAV-scoped). App-password authentication establishes no web session cookie, and vice versa — the two auth paths are independent so a revoked app password never logs out a browser session and a web logout never invalidates an app password.
- REST API uses session cookies (web client) or bearer tokens (programmatic).

**Authorization:**
- Permissions are scoped to organizations. A user's capabilities within an org are determined by `Membership.role`.
- No system-wide application roles. An optional `User.is_system_admin` boolean covers instance-level operations (org provisioning, user management).
- A `PartAssignment` implicitly grants the assigned user **read access to that voice's files** for as long as the assignment exists, regardless of Membership in the org. This is how guests and substitutes access parts.

### HTTP & REST API conventions

These govern the JSON REST tree (`/v1/...`). The HTMX admin tree returns HTML fragments and is exempt where noted.

- **Pagination.** List endpoints (arrangements, voices, files, members, audit log, …) use **offset/limit**: `GET /v1/arrangements?limit=50&offset=0`. Response is an envelope: `{ "items": [...], "total": N, "limit": L, "offset": O }`. Default `limit` = 50, max = 200 (configurable via `LIED_MAX_PAGE_SIZE`). Cursor pagination was rejected for phase 1: offset gives a cheap `total` for "page 3 of 7" UIs and the concurrent-insert inconsistency window is irrelevant at our scale. Switching to an optional cursor later is additive, not breaking.
- **Optimistic concurrency.** Mutating requests use **ETags derived from `updated_at`** (millisecond epoch). GET returns `ETag: "<ms>"`; `PATCH`/`PUT`/`DELETE` MUST send `If-Match`; a stale value → **`412 Precondition Failed`**. Applies to every entity carrying `updated_at` (Arrangement, Voice, File metadata, Collection, CollectionItem, GlobalAnnotation). HTMX forms carry the ETag in a hidden field. A dedicated `version` column was rejected as redundant — `updated_at` already exists everywhere and ms precision catches concurrent edits.
- **Error response shape.** All `/v1` errors use **RFC 7807 Problem Details** (`application/problem+json`): `{ "type", "title", "status", "detail", "instance" }`, where `type` is a stable URI (e.g. `https://lied/errors/precondition-failed`) and `instance` carries the request-ID for log correlation. The single top-level `AppError` (see Error handling) emits this. HTMX error responses are HTML fragments, not Problem Details.
- **CSRF.** Cookie-authenticated state-changing requests (the HTMX admin tree) require a **double-submit token**: a session-bound token in a cookie, copied into an `HX-CSRF` header by a global `htmx:configRequest` handler and validated by middleware. `SameSite=Lax` on the session cookie is a backstop, not the primary defense. Bearer-token `/v1` REST is exempt (not cookie-auth, so not CSRF-able).
- **API versioning.** JSON REST is served under a **`/v1` prefix from day 1**. The HTMX admin tree (`/admin`) is unversioned — it's server-rendered, ships with the binary, and has no external contract to break. `/v1` is the clean seam for the future federation/SaaS API.
- **Route-tree boundary.** Four sibling trees share one composed middleware stack (request-ID, rate-limit, auth), each applied with the right extractor — no content-type sniffing:
  - `/admin/...` → HTML fragments (HTMX; session cookie + CSRF).
  - `/v1/...` → JSON (session cookie *or* bearer token).
  - `/orgs/...`, `/users/.../library/...` → WebDAV (app-password auth).
  - `/healthz`, `/readyz`, `/metrics` → infra, unauthenticated (see below).
- **Health, readiness, metrics** (infra tree, outside `/v1`):
  - `/healthz` — liveness; returns 200 whenever the process is up. Drives container-restart decisions.
  - `/readyz` — readiness; checks Postgres (`SELECT 1`) and MinIO (`HeadBucket`) reachable, 503 if either is down. Drives traffic gating without triggering restarts, so a transient DB blip sheds load instead of cycling the container.
  - `/metrics` — Prometheus format via `metrics` facade + `metrics-exporter-prometheus` (facade kept swappable for OTLP later; not coupled to axum internals). RED baseline (request rate, error rate, per-route duration histograms) plus upload/download byte counters. **Gated behind `LIED_METRICS_ENABLED` (default off)** since the endpoint is unauthenticated — an operator opts in rather than leaking operational detail from a fresh self-host.
- **Sort & filter.** List endpoints accept `?sort=<field>:<dir>` (e.g. `sort=created_at:desc`) and `?filter[<field>]=<value>` (e.g. `filter[status]=active`); multiple filters AND together. Each endpoint declares an **allowlist** of sortable/filterable fields with a default sort (arrangements default `title:asc`); an unknown field or direction → `400` Problem Details. The allowlist keeps this from becoming arbitrary-column SQL surface. The bracket syntax namespaces filters away from control params (`sort`, `limit`, `offset`, `q`) so they never collide. Phase-1 ILIKE search stays a separate `?q=` param.
- **OpenAPI exposure.** `utoipa` serves the spec at **`/openapi.json` (always available)**; a rendered UI via `utoipa-rapidoc` is mounted at **`/docs`, gated behind `LIED_DOCS_ENABLED` (default on)**. Docs default on (unlike metrics) because the API spec isn't sensitive operational data and discoverability helps the programmatic-client/federation story; an operator who wants it dark flips one flag. RapiDoc over Swagger UI keeps the binary slim (one embedded asset).

### WebDAV layout

Canonical directory tree (also the public contract for external tools mounting the volume):

```
/orgs/<org-slug>/
    arrangements/
        <arrangement-slug>/
            score/                          ← full-score files
                <name>.<ext>                   (.pdf, .musicxml, .ly, .png, …)
            voices/
                <voice-slug>/
                    <name>.<ext>               ← canonical voice files
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

- All path segments are **immutable slugs** stored on the corresponding entity (`Organization.slug`, `Arrangement.slug`, `Voice.slug`, `User.slug`, `Collection.slug`). Slugs are generated from the name/title at creation and do not change when the display name is edited. Renaming a slug is a separate, explicit REST operation (admin-gated: requires `Membership.role = owner` for org-scoped entities, or `is_system_admin` for User and Instrument), audited, and reserved for fixing typos. It performs an atomic move of all affected MinIO objects to the new prefix and updates the entity's `slug` field in one transaction; cached WebDAV mounts pointing at the old slug will break.
- Soft-deleted rows (`deleted_at != NULL`) are hidden from WebDAV listings.
- The `/orgs/<org>/collections/` subtree is a read-only computed view. Writes go through `/orgs/<org>/arrangements/`.
- **Collections subtree visibility (auth-filtered):**
  - `owner` / `archivist` / `conductor` see every voice + the full score for each `CollectionItem` — supports program assembly, printing, and the principal coverage workflow.
  - `musician` sees only the files for voices where they have a `PartAssignment` on that `CollectionItem`. The full score is invisible.
  - Guests (PartAssignment without Membership) get the musician's view scoped to their assigned voice(s).
  - The same files are reachable under `/orgs/<org>/arrangements/...` with the same per-role filtering; the collections subtree is the "what's in this concert, in order" view, not a separate permission domain.
- **PROPFIND scope at the arrangements root.** A `musician` (or guest/substitute) listing `/orgs/<org>/arrangements/` sees **only arrangements where they hold at least one `PartAssignment`** — unassigned arrangements are simply absent, not shown as empty/403 placeholders. Staff roles (`owner`/`archivist`/`conductor`) see all. Rationale: showing every title with locked subdirs would leak the org's full repertoire and naming to guests and clutter a mounted tablet view with hundreds of inaccessible folders; discoverability of "what the org owns" belongs in the admin UI, not the filesystem view. Consistent with the collections-subtree filtering above.
- `/users/<user>/library/` is private to that user, not bound to any org.

### Display clients (deferred)
Musicians can use existing apps against the WebDAV interface:
- **PDF readers on tablet:** forScore, MobileSheets, Xodo (performance use)
- **Notation software:** MuseScore (MusicXML), Frescobaldi (LilyPond)
- **OS file manager:** for general browsing and access

A purpose-built display/reader app is a future consideration, not an initial requirement.

---

## Tech stack

Concretizes the Architecture section. The backend is Rust + axum, the admin UI is HTMX + maud, packaged as a Docker image.

### Backend
- **`axum`** — web framework. All HTTP routing for REST + HTMX endpoints.
- **`sqlx`** (`postgres` feature) — async DB driver with compile-time-checked SQL against a real database. SQL is hand-written; no ORM. **Offline mode**: `.sqlx/` (the prepared-query cache produced by `cargo sqlx prepare`) is committed to the repo; `SQLX_OFFLINE=true` is set in CI and in the Docker builder stage so builds are hermetic and need no DB. A CI gate runs `cargo sqlx prepare --check` against the dev DB to catch out-of-sync caches.
- **`sqlx-cli`** — migrations as plain `.sql` files in `/migrations`, embedded in the binary at build time.
- **`dav-server`** — WebDAV crate; mounted under `/orgs/...` and `/users/<user>/library/...`.
- **`aws-sdk-s3`** — official AWS SDK; speaks MinIO natively.
- **`argon2`** — password hashing for local accounts.
- **`tower-sessions`** — session cookies for the web client. Backend: **Postgres-backed store** (`tower-sessions-sqlx-store`); the `sessions` table is added via our `/migrations` directory. Chosen over in-memory because the NFR section says scaling must not be precluded; chosen over Redis to avoid a second service. Each authenticated request reads one row — negligible at our scale; if it ever becomes hot, swap to a Redis-backed store (see Infrastructure pluggability rule in NFR posture).
- **`jsonwebtoken`** — bearer tokens for programmatic REST clients. **Algorithm: HS256** (symmetric); Lied is the only issuer and verifier, so asymmetric is wasted complexity. The signing secret is 32 random bytes managed via the secrets layer with the hot-rotation flow already specced (new key written, old key honored for a grace window). Every token carries a **`kid` header** naming the signing key; during the rotation grace window both keys live in the verifier's keyring and the verifier selects by `kid` rather than trial-verifying against each (which would be wasteful and ambiguous in logs). New tokens are signed with the newest key. Token lifetime: **30 days, configurable.** Claims: `sub` (user id), `org` (active org id, nullable for system-admin operations), `iat`, `exp`, `jti` (id for future revocation listing). If federation ever needs external verification, switching to ES256 is a contained migration (accept both during cutover, retire HS256).
- **`tower-governor`** — rate limiting as axum/tower middleware.
- **`tracing`** + **`tracing-subscriber`** — structured logging.
- **`figment`** — layered config (file + env + the `_FILE` / `cmd:` secret-source pattern from the NFR section).
- **`utoipa`** — derives OpenAPI from the axum routes; the API spec doesn't drift from the code.
- **`uuid`** (`v7` + `serde` features) — UUIDv7 primary keys (see Implementation conventions).
- **`garde`** — derive-based input validation; failures map to a 400 Problem Details (see Implementation conventions).
- **`thiserror`** for typed domain errors; **`anyhow`** in the binary entrypoint.

### Admin UI
- **HTMX** for interactivity; server returns HTML fragments.
- **`maud`** for templates — compile-time HTML macros in Rust syntax. Auto-escapes by default; any `PreEscaped` use needs a justification comment.

### Async runtime
- **`tokio`** (1.x).

### Build / packaging
- **Multi-stage `Dockerfile`** — builder runs `cargo build --release`; final image is `gcr.io/distroless/cc-debian12` (~20MB, distro-agnostic).
- **`docker-compose.yml`** for dev: Postgres + MinIO + the app, with hot reload via `cargo watch`.

### Testing
- **`cargo test`** native runner.
- **`testcontainers`** for ephemeral Postgres + MinIO in integration tests; no fixture pollution between tests.
- **`insta`** for snapshot tests of REST response shapes.

### Deferred (phase 2+)
- **Audiveris** (Java, separate container) — OMR for PDF/image → MusicXML.
- **`openidconnect`** — OIDC client when SSO becomes a need.

---

## Non-functional posture

For the first few iterations the operational target is "as fast and cheap as possible." Hard NFR numbers (latency budgets, availability targets, scale ceilings) are not pinned yet — they get pinned as use grows. The constraint on the design is that none of the choices above should foreclose future scaling, multi-tenancy, or hosted operation.

### Deployment topologies

Lied does not assume a particular self-hosting entity. The realistic shapes:

1. **One org self-hosts for itself.** An orchestra's IT runs the container for its own members. Multi-org concepts are vestigial here — one org per instance.
2. **A federation / umbrella body self-hosts for several related orgs.** Music schools with multiple ensembles, regional band associations, conservatories. Member orgs are colleagues; sharing catalog entries is a feature, not a leak.
3. **Neutral hosting provider runs Lied as SaaS** — strict tenant isolation between unrelated orgs. **Future scenario; explicitly out of scope for phase 1.**
4. **A single individual self-hosts for personal practice.** Mostly uses `/users/<user>/library/`.

**The trust assumption phase 1 relies on:** every org in one instance trusts the other orgs in the same instance. This holds for topologies 1 (only one), 2 (allied), and 4 (only one). Topology 3 violates it and requires a tenant-isolation pass before being supported.

### Infrastructure pluggability rule

Anything that stands in for "external system that could be swapped" (session store, rate-limit store, future queue, future cache) is accessed *only* through its crate's trait abstraction. Application code never sees the concrete backend. Specifically: **never JOIN infrastructure tables (e.g. the `sessions` table) with domain tables.** Treat them as opaque key/value stores accessed via the trait. This keeps in-memory → Postgres → Redis migrations to a one-line wiring change with a forced reset where appropriate (sessions: re-login; rate limits: bucket reset), not a data migration.

### Operational stance
- **Target deployment:** single self-hosted instance, small numbers of orgs and users — covering topologies 1, 2, and 4.
- **Configurable limits.** Whenever a limit is imposed (max upload size, rate-limit window, per-org storage cap, conversion job timeout, etc.) it MUST be documented and configurable. Hardcoded limits are not acceptable.
- **Phase 1 backup posture (minimum bar).** The README documents:
  - **Postgres:** `pg_dump --format=custom` to a host-mounted volume on a cron schedule; `pg_restore --clean` for recovery.
  - **MinIO:** `mc mirror` to a second MinIO instance or host directory on the same schedule; reverse mirror for recovery.
  - **Ordering:** dump **Postgres first, then MinIO**; restore in the same order. The DB is the authority for existence, so the safe failure mode is a DB reference to a not-yet-mirrored file (a detectable 404) rather than a MinIO object with no DB row (dead bytes, non-corrupting). This is best-effort, not a consistent snapshot — acceptable at this posture; true snapshot coordination is a phase-2 runbook item.
  - **Required volume mounts** in `docker-compose.yml`: Postgres data dir, MinIO data dir, backup target dir, all host-mounted.
  - **Back up:** those three volumes plus the `.env` / secrets files.
  - **OK to lose on restore:** in-memory rate-limit state (acceptable), session cookies (users re-login).
  - A `just backup` target wraps the two commands against the docker-compose volumes.

  **Not promised in phase 1:** point-in-time recovery, continuous WAL streaming, cross-region replication, automated restore verification, a backup-orchestration tool. Those are phase-2 runbook items.

### Default limits (all configurable)

| Limit | Env var | Default | Rationale |
|---|---|---|---|
| Max single file upload | `LIED_MAX_UPLOAD_BYTES` | 200 MB | Covers 99.9th-percentile orchestral PDFs; anything larger is almost always a mistake or needs splitting |
| Max non-upload request body | `LIED_MAX_REQUEST_BYTES` | 256 KB | JSON / form bodies; prevents bomb attacks |
| Max files per voice | `LIED_MAX_FILES_PER_VOICE` | 50 | Sanity guardrail; never normally reached |
| Max arrangements per org | `LIED_MAX_ARRANGEMENTS_PER_ORG` | unset (no limit) | Reserved for SaaS-mode quotas later |
| Max list page size | `LIED_MAX_PAGE_SIZE` | 200 (default 50) | Offset/limit pagination ceiling (see HTTP & REST API conventions) |
| Rate limit, authenticated | `LIED_RATELIMIT_AUTH_PER_MIN` | 120 req/min | HTMX UIs fire several requests per interaction |
| Rate limit, unauthenticated | `LIED_RATELIMIT_ANON_PER_MIN` | 20 req/min | Per source IP |
| Rate limit, login/password | `LIED_RATELIMIT_LOGIN_PER_MIN` | 10 req/min | Per IP; brute-force defense |
| Metrics endpoint | `LIED_METRICS_ENABLED` | off | Unauthenticated `/metrics`; operator opts in |
| API docs UI | `LIED_DOCS_ENABLED` | on | RapiDoc UI at `/docs`; `/openapi.json` always served |

**Two tiers of configuration — classify by *who owns the value*, not by what's easiest to edit.**

- **Operator config (env / `_FILE` / `cmd:` via figment; never in the DB or any UI).** Everything that protects the *deployment* — host, process, bandwidth, auth surface: all rate limits, max upload/request bytes, page-size ceiling, JWT lifetime, signing keys, all secrets, the metrics toggle. Owned by whoever runs the container (deploy/filesystem access), not by an org `owner` or even `is_system_admin`. Two reasons it must not move into the admin UI:
  1. **Trust boundary.** In the federation/SaaS topology the org admin is exactly the party being rate-limited and size-capped — the limited party must not be able to raise its own ceiling.
  2. **Mechanics.** These are wired into startup middleware (`tower-governor`, body-size layers); read-once-at-boot is trivial, live DB reload is plumbing for zero phase-1 benefit. They change rarely — a restart is acceptable.

  The "every limit MUST be documented and configurable" rule above is satisfied by env config; *configurable* ≠ *editable by an org admin*.
- **Org policy (DB row / `org_settings`, edited in the admin UI by `owner`).** Only genuine per-org *business policy* that is **not** a resource/abuse protection. In phase 1 this set is nearly empty; candidates as they arise: per-org storage quota (SaaS-mode, deferred), default difficulty scale, default tag vocabulary, display preferences. These live on the `Organization` row or a small settings table.
- **Hybrid (deferred):** if an org ever needs to tune a Tier-1 limit, the env var sets the operator's **hard ceiling** and the org setting may only make it **stricter** — enforced value = `min(operator_cap, org_setting)`. An org admin can tighten its own limits, never loosen them. Phase 1 needs only the env vars; this pattern is noted so the boundary isn't violated later.

**Uploads stream end-to-end.** axum's `extract::Multipart` reads the body as a stream; chunks flow straight to MinIO via the S3 `UploadPart` API. Server memory per upload is fixed at the chunk buffer size (a few MB), not file size. The same applies to downloads: aws-sdk-s3's `GetObject` returns a `ByteStream` piped directly into the axum response body, with HTTP `Range` requests honored so tablets can seek inside large PDFs.

### Security baseline
- **Rate limiting** is built in from day 1, with per-route and per-identity buckets configurable per deployment. Defaults (all env-overridable):
  - Authenticated identity: **120 req/min** (`LIED_RATELIMIT_AUTH_PER_MIN`) — HTMX UIs fire several requests per interaction, so the floor is generous.
  - Unauthenticated IP: **20 req/min** (`LIED_RATELIMIT_ANON_PER_MIN`).
  - Login / password endpoints: **10 req/min per IP** (`LIED_RATELIMIT_LOGIN_PER_MIN`) — stricter; this is the brute-force surface.
  - WebDAV is exempt from the per-request limiter (chatty PROPFIND/LOCK traffic) but subject to a generous per-identity ceiling.
- **Audit logging** is shallow but present from day 1. The rule is **every write to any persistent entity is logged** — not an enumerated list. Concretely this covers:
  - **Auth events:** login (success/fail), logout, password change, OIDC link, app-password create/use/revoke.
  - **Authorization events:** Membership create/update/delete, role change, principal-flag change, instrument-assignment change.
  - **Content writes:** create / update / soft-delete / hard-delete / undelete on Arrangement, Work, Voice, File, Collection, CollectionItem, PartAssignment (including `notified_at`/`acknowledged_at`), Tag, ArrangementTag, GlobalAnnotation, Instrument.
  - **Admin events:** org create/delete, user create/delete, system-admin grants, slug renames, secret rotation events.
  - **Reads are NOT logged** in phase 1 — too noisy, low signal. Reserved for phase 2 if compliance requires.

  Storage: `audit_log` Postgres table in the same DB. Columns: `id`, `at` (timestamptz), `actor_user_id` (nullable for system events), `org_id` (nullable for instance-level), `action` (text, e.g. `arrangement.create`, `membership.role_change`), `target_kind`, `target_id`, `payload` (jsonb — diff or context, secrets redacted), `request_id` (correlates with the tracing span). For writes to instance-wide entities (`Work`, `Instrument`), `org_id` is `NULL` regardless of which org the actor was operating from — instance-wide events are not attributed to an org.

  **Redaction policy.** Before any `payload` is written, a single redaction pass in the `audit()` helper replaces a static deny-list of field names with the sentinel `"[redacted]"` (key retained so the diff still shows the field changed): `User.password_hash`, `AppPassword.hash`, `AppPassword` plaintext token, JWT/session signing keys, OIDC client secret, and any value sourced via the `cmd:` / `file:` secret resolvers. This is the second line of defense — secret reads already route through the config layer, which redacts at that boundary (see Secrets management). A deny-list (not allow-list) is used because payloads are arbitrary jsonb; the config-layer redaction backstops anything a new secret field misses.

  Append-only by convention; the only mutation is an admin-only redact that overwrites `payload` for a single entry and logs the redaction itself. Inline storage keeps the single-binary self-hostable story intact; a phase-2 retention/archival policy can ship old rows to a file or S3 if volume warrants. Phase 1 implementation is a single `audit(action, target, payload)` helper called from every write site — discipline + PR review, no macros yet.
- **Secrets management:**
  - All secrets are read from environment variables. Every secret env var `X` also accepts `X_FILE` pointing at a path — the Docker secrets convention, which works with Docker Compose/Swarm secrets, Kubernetes secrets, and systemd `LoadCredential=`.
  - The config layer abstracts secret sources so each secret can declare `env:`, `file:`, or `cmd:` (shells out to a helper like `pass`, `op read`, `vault kv get`). Vault / Infisical / 1Password / Bitwarden plug in later without coupling the app to any of them.
  - Secret values are redacted from logs and error responses at the config-layer boundary, not at every call site.
  - Session/JWT signing keys support hot rotation via an admin REST endpoint (new key written, old key honored for a grace window). DB/MinIO credentials tolerate restart for rotation.
  - **Anti-patterns:** no mandatory Vault dependency, no secrets committed to the repo (provide `.env.example` only), no custom encrypted-secrets-file format.

---

## Development workflow & quality bar

Designed to give agentic loops fast, mechanical self-verification, while not adding friction that pays no real dividend.

### CI gates (every PR must pass)
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all`
- `cargo deny check` — license + security policy on dependencies
- `cargo sqlx prepare --check` — verifies `.sqlx/` cache matches the queries in code
- Docker image builds successfully (with `SQLX_OFFLINE=true`)

### Local verification
- **`just`** is the build tool; `justfile` lives at the repo root.
- **`just check`** runs fmt + clippy + test — the canonical command an autonomous loop runs to verify its work.
- Other targets: `just fmt`, `just lint`, `just test`, `just migrate`, `just prepare` (refreshes `.sqlx/` after SQL changes), `just dev` (`docker compose up` + `cargo watch`), `just build` (release binary), `just image` (docker image build), `just backup` (runs `pg_dump` + `mc mirror` against the docker-compose volumes).

### Toolchain pinning
- **`rust-toolchain.toml`** at the repo root pins the Rust version explicitly. Example:
  ```toml
  [toolchain]
  channel = "1.86.0"          # pin a specific release, never "stable"
  components = ["rustfmt", "clippy"]
  profile = "minimal"
  ```
- **MSRV** = the pinned channel. We don't support older Rust versions; `rustup` will fetch the pinned toolchain automatically on first build.
- **Bump cadence:** deliberate, in a dedicated PR, when there's a real reason (language feature, clippy regression fix, dependency requirement). Not automatic — pinning is the whole point.

### Lied's license and dependency policy

- **Lied is published under `MIT OR Apache-2.0`** — the standard Rust-ecosystem dual license. Maximally permissive, matches the deny.toml allowlist, no friction for self-hosters or downstream packagers. AGPL was considered for SaaS-fork protection but rejected as inappropriate for a learning project; revisiting is non-breaking if Lied ever competes with a hosted offering.
- **`deny.toml` policy** (the `cargo deny check` CI gate):
  - **License allowlist:** `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `MPL-2.0`, `Unicode-DFS-2016`, `Unicode-3.0`, `Zlib`, `CC0-1.0`, `Unlicense`.
  - **Explicitly excluded:** copyleft licenses (GPL, LGPL, AGPL) and any "non-commercial" / "evaluation" terms — pulling them in would constrain Lied's own license.
  - **Advisories:** `vulnerability = "deny"`, `yanked = "deny"`, `unmaintained = "warn"`.
  - **Duplicates:** `multiple-versions = "warn"` — not a hard fail, but visible.
  - **Exception list:** starts empty; any `(crate, license)` exception requires a one-line justification comment in `deny.toml`.

### Type safety / strictness
- `#![forbid(unsafe_code)]` at the crate root.
- `unwrap()` / `expect()` in production paths trigger clippy warnings (so they fail CI). Tests may use them freely.

### Implementation conventions

Structural choices locked before coding so issues don't bake in conflicting decisions.

- **Primary keys: `uuid` v7.** All entity PKs are UUIDv7 (time-ordered → good B-tree insert locality and rough creation ordering for free). Generated app-side via the `uuid` crate. No `bigserial`.
- **REST resource identifier: UUID in `/v1` paths.** Endpoints address entities by UUID (`/v1/arrangements/{uuid}`), never by slug. Slugs are *scoped* (per-org / per-arrangement) and *renameable* (admin slug-rename op), so they're unsuitable as stable REST URLs; they remain the human-readable keys in the WebDAV tree only. A future slug lookup, if needed, is a `?slug=` resolver, not the canonical path.
- **JSON field casing: `camelCase`.** Wire JSON uses `camelCase`; Rust structs stay `snake_case` with `#[serde(rename_all = "camelCase")]` applied consistently. The `utoipa` schema reflects the same casing so the OpenAPI contract matches the wire.
- **Crate layout: workspace with a `lib` target.** The app is a workspace whose core is a library crate (`lied`) with a thin binary (`lied-server`) on top. Integration tests under `tests/` link the lib and may exercise internals directly, not only black-box HTTP — this is what the fixture builders (Testing posture) depend on.
- **Enums: `text` + `CHECK`, not Postgres native enum types.** All status/type/kind columns (`Membership.role`, `Arrangement.status`, `Collection.type`, `File.format`, `File.conversion_quality`, …) are stored as `text` with a `CHECK (col IN (...))` constraint, mapped to Rust enums via sqlx. Native PG enum types were rejected: `ALTER TYPE ... ADD VALUE` is awkward (can't run in a transaction, can't remove values), and adding a variant should be a plain migration. `Tag.kind` stays free-text by design (no CHECK).
- **Validation: `garde` derive.** Input DTOs validate via `garde`; a single `garde::Report` → `AppError` mapping at the extractor boundary turns failures into a `400` Problem Details carrying a field-error extension member (`errors: { field: [messages] }` — RFC 7807 permits extensions). Uniform across every endpoint.

### Error handling
- **`thiserror`** for typed error enums in domain code (errors have shape).
- **`anyhow`** in the binary entrypoint and one-off scripts.
- One top-level `AppError` implements `axum::response::IntoResponse` so errors become HTTP responses consistently. For the `/v1` JSON tree it serializes as RFC 7807 Problem Details (`application/problem+json`); see HTTP & REST API conventions.

### Logging
- `tracing` everywhere; never `println!` / `eprintln!` in production code.
- JSON output in production, pretty in dev (toggled via env var).
- Every request gets a request-ID span; logs use structured fields, never string interpolation.

### Testing posture
- **Unit tests** colocated with code; fast, no DB.
- **Integration tests** under `tests/`; use `testcontainers` for real Postgres + MinIO.
- **Fixtures** are composable builder helpers (`seed_test_org() -> OrgCtx`, `seed_arrangement(&org)`, `seed_user_with_role(...)`) returning typed handles, layered over a fresh per-test container/schema — not raw per-test SQL. Builders go through the real repository functions so they stay correct as the schema evolves; raw SQL fixtures drift from the actual insert paths and rot.
- New code without tests is a PR objection, not a hard CI gate (chasing coverage % produces bad tests).

### Branching & PR flow
- `main` stays linear — no merge commits, ever.
- One issue → one branch (`issue-N-short-slug`) → one PR.
- During development, use `git commit --fixup <sha>` / `--squash <sha>` to direct follow-ups at earlier commits.
- Before opening the PR, run `git rebase -i --autosquash <base>` to clean the branch into its final commit sequence. A PR with `fixup!` / `squash!` commits left in is not ready.
- Repo-level git config: `pull.rebase = true`, `rebase.autosquash = true`.
- GitHub merge mode: **Rebase and merge** (not squash, not merge commit). PRs may carry multiple meaningful commits if the history reads cleanly.
- PR title references the issue; PR body has the acceptance-criteria checklist + `Closes #N`.

### Commit messages
- Free-form, imperative mood, matches existing style.
- Co-author footer when an agent did substantial work.
- No conventional-commits enforcement.

### Definition of done (issue can close)
1. Acceptance criteria in the issue body all checked.
2. CI green (fmt + clippy + test + deny + image build).
3. Migration applies cleanly from scratch, if schema changed.
4. Public-API docs updated (rustdoc on public items) if behavior changed.
5. Branch rebased clean (no `fixup!` / `squash!` left).
6. CLAUDE.md updated if a design decision shifted.

### Documentation
- `rustdoc` on public items in any future library crates.
- `utoipa` generates OpenAPI from the routes; no separate hand-written API doc.
- CLAUDE.md is the design spec.
- README is minimal: clone, `docker compose up`, open the UI.

### Security minima (beyond the NFR section)
- SQL only via `sqlx::query!` / `sqlx::query_as!` (compile-time-checked, parameterized). String-formatted SQL is a PR blocker.
- XSS: `maud` auto-escapes; `PreEscaped` requires a justification comment.
- Secrets never logged: enforced by a single `Debug` impl on the config struct that redacts secret fields.

### Not in the bar
- No coverage threshold.
- No conventional-commits / changelog automation.
- No mandatory ADRs.
- No mutation / fuzz / property-based testing requirements (allowed, not required).

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

#### Scope & ownership
- **Provenance** is flat metadata on `Arrangement` (publisher, arranger, purchase date, license notes, copy count). No separate publisher/license entity unless querying by publisher becomes a clear need.
- **Editions/variants (UC-19, UC-20).** Each `Arrangement` is already a specific edition. Multiple editions of one work = multiple `Arrangement` rows sharing a `Work`. `Arrangement.status` (`active`, `archived`) lets an org retire an old edition without deleting it; multiple editions may be active simultaneously.
- **Work is instance-wide.** `Work` rows are not scoped to an organization — the abstract piece is the same everywhere. In the federation topology (NFR posture) this is a feature: orgs reuse each other's catalog entries. No `organization_id` on Work; no `deleted_at` either (Works are durable references; orphans are not garbage-collected). When SaaS topology becomes a goal, a nullable `organization_id` can be added — `null` = shared, non-null = private — backwards-compatible.
  **Permissions:** *creating* a Work is open to any authenticated user with at least one Membership in any org (an archivist adding a new piece's abstract entry); *editing* a Work is restricted to its original creator (`created_by`) or any `is_system_admin`. This stops "alice fixes a typo and rewrites a shared title for everyone"; for legitimate edits in someone else's Work, escalate to a system admin.
- **Tags.** Open-ended dimensions (theme, mood, era, occasion, style) live in a per-org `Tag` entity with a free-text `kind` and a many-to-many `ArrangementTag` join. Per-org so vocabularies don't leak between orgs; `kind` is text (not enum) so orgs can invent categories without a migration.
- **Instruments are a controlled vocabulary.** Instance-wide `Instrument` table seeded on first migration with ~150 standard instruments (IMSLP/MuseScore canonical list). `Voice.instrument_id`, `Membership.instrument_ids`, and `Membership.principal_instrument_ids` all reference `Instrument` by FK / FK-array. Free text was rejected because the coverage check (`Voice.instrument_id ∈ Membership.principal_instrument_ids`) requires reliable equality; a hard-coded enum was rejected because non-Western and historical instruments would need code releases. **Permissions:** adding or editing an Instrument requires `User.is_system_admin = true` (the entity is instance-wide; per-org users shouldn't be able to inject vocabulary that affects every org). Reading the Instrument list is open to any authenticated user.

#### Lifecycle, existence, audit
- **DB is source of truth for existence; MinIO versioning is content history only.** Setting `deleted_at` hides a row from REST, WebDAV, and search; the MinIO object remains. Undelete clears `deleted_at` — no MinIO operation. Hard delete (admin-only) removes both the DB row and all MinIO object versions. MinIO version history is reachable through REST admin endpoints but never surfaced via WebDAV.
- **Soft delete** via `deleted_at` on Arrangement, Voice, File, Collection, CollectionItem, Tag.
- **Soft-delete cascade: hide-with-references.** A soft-delete sets `deleted_at` on the targeted entity only — never on its descendants. Queries that join parent → child filter `WHERE parent.deleted_at IS NULL AND child.deleted_at IS NULL`, so a soft-deleted parent makes its subtree invisible *through it* without changing the children's own state. Undelete just clears the parent's `deleted_at` and the subtree reappears automatically. CollectionItems / PartAssignments that reference a soft-deleted target remain in the collection but render as "[removed]" with the slug shown — broken references are surfaced, not silently dropped. Soft-cascade and block-on-references were both rejected: soft-cascade makes undelete ambiguous (was the child already deleted before, or only by cascade?); block-on-references creates bad UX ("can't delete until you remove from 7 collections").
- **Audit fields.** `created_at`, `updated_at`, `created_by` (user FK, nullable for system) on Arrangement, Voice, File, Collection, CollectionItem, GlobalAnnotation.
- **Time zones.** Two rules:
  1. **All instants are `timestamptz`, stored and serialized in UTC** (ISO 8601 with `Z` suffix: `2026-05-16T14:30:00Z`). Postgres `timestamptz` stores UTC internally regardless of session TZ. Rust uses `chrono::DateTime<Utc>`. Covers: `created_at`, `updated_at`, audit-log `at`, `last_used_at`, `revoked_at`, `notified_at`, `acknowledged_at`, session expiry, JWT `iat`/`exp`.
  2. **Civil dates** (calendar facts without a time) use `date`. Currently only `Arrangement.purchase_date` — "we purchased this on 2026-04-12" is a fact about a calendar day, not a UTC instant.

  Display happens in the user's local TZ — browser detects, server returns UTC, client formats. Server never guesses a display TZ. **Datetime crate: `chrono`** with `serde` + `clock` features (broader ecosystem integration than `time` for sqlx/axum/utoipa).
- **Unique constraints** (all are partial indexes `WHERE deleted_at IS NULL` where the entity has soft-delete, so the constraint applies to live rows only):

  | Entity | Constraint |
  |---|---|
  | `Organization` | `slug` |
  | `User` | `slug`; `username`; `email` (partial, where not null) |
  | `AppPassword` | `(user_id, name)` |
  | `Instrument` | `key` |
  | `Arrangement` | `(organization_id, slug)` |
  | `Voice` | `(arrangement_id, slug)` |
  | `File` | `(arrangement_id, voice_id, name, format)` for voice files; `(arrangement_id, name, format) WHERE voice_id IS NULL` for full-score files (Postgres treats NULL as distinct, so a partial index covers the score case) |
  | `Membership` | `(user_id, organization_id)` |
  | `Collection` | `(organization_id, slug)` |
  | `CollectionItem` | `(collection_id, index)` |
  | `PartAssignment` | `(collection_item_id, voice_id)` — one assignee per voice per item; reassignment replaces the row |
  | `Tag` | `(organization_id, name, kind)` |
  | `ArrangementTag` | `(arrangement_id, tag_id)` |
  | `Work`, `GlobalAnnotation` | none — duplicates allowed (Work duplicates resolved via phase-2 merge tooling; multiple GlobalAnnotations per arrangement is the norm) |

  Phase-1 design: `PartAssignment` permits only one user per voice per item. Cover-player / doubling support would relax this by adding a `role` discriminator on the row.

#### Files & storage
- **Conversion provenance.** `File.derived_from_file_id` (nullable FK) links a derived file to its source; a file with no `derived_from` is a source. `File.conversion_quality` (`clean`, `omr`, `manual`) surfaces UC-10's lossless-vs-approximate distinction. No separate `Conversion` table — the FK + quality enum is sufficient until we need failure history.
- **Atomic file replacement.** Replacing a file (REST or WebDAV `PUT` over an existing path) is **never an in-place mutation**: it inserts a new `File` row and soft-deletes the old one in a single transaction. This preserves the `derived_from_file_id` chain and audit history. The derived MinIO object key is identical, so MinIO versioning captures the content history while the DB row swap captures identity. In-place update was rejected because it would destroy provenance and contradict the "files are immutable representations" model.
- **Multiple full-score files per arrangement.** Permitted: an Arrangement can have several files with `voice_id IS NULL` (e.g. LilyPond source + publisher PDF + annotated conductor copy), disambiguated by `(name, format)`. No `primary_score` flag in phase 1 — consumers pick by convention: source-format preferred (LilyPond → MusicXML → PDF), earliest `created_at` to break ties; UI offers the full list when there's choice. Adding `Arrangement.primary_score_file_id` later is a non-breaking migration if the convention proves insufficient.
- **Storage path is derived, not stored.** The MinIO object key for a `File` is computed deterministically from the entity tree:
  - Voice file: `orgs/<org-slug>/arrangements/<arr-slug>/voices/<voice-slug>/<file-name>.<ext>`
  - Full-score file: `orgs/<org-slug>/arrangements/<arr-slug>/score/<file-name>.<ext>`
  - Extension is derived from `File.mime_type`.
  Slug immutability (see WebDAV layout) keeps the path stable; the rare "rename slug" admin operation moves all affected MinIO objects to the new prefix in one operation. WebDAV writes resolve path segments to entity slugs and insert `File` rows; WebDAV reads recompute the path from the row. The same key is also browsable via MinIO's own web UI / `mc` CLI without going through Lied — a small ops bonus.
  **Personal annotations are out of scope of this rule** — they are files-by-convention with no `File` row backing them. Their path (`…/voices/<voice-slug>/annotations/<user-slug>/<name>.pdf`) is governed by the personal-annotations naming convention below, not the derived-path rule.
- **Image subtype.** `File.mime_type` (text) records the actual subtype (`image/png`, `image/jpeg`, `image/tiff`, …). The `format` enum stays coarse and drives routing (e.g. `image` → display-only).
- **`format` ↔ `mime_type` relationship.** `format` is the coarse routing key (drives conversion eligibility, display logic); `mime_type` is the precise IANA type. `format` is derived from `mime_type` at insert/update time via a lookup table, then stored alongside for fast filtered queries. The invariant: `format` MUST agree with `mime_type` (e.g. `mime_type = image/png` ⇒ `format = image`); rows where they disagree are a bug. The lookup table is implementation detail (e.g. `application/x-lilypond` → `lilypond`, `application/vnd.recordare.musicxml` → `musicxml`, `image/*` → `image`, etc.); the extension used in MinIO paths derives from the same table.

#### Annotations & distribution state
- **Distribution (UC-12) and coverage (UC-13).** No separate distribution table; `PartAssignment.notified_at` and `PartAssignment.acknowledged_at` (both nullable timestamps) carry the state. Coverage check: every required voice has a `PartAssignment`; "rehearsed" coverage additionally requires `acknowledged_at`.
- **Personal annotations** are files in MinIO, no DB record. Naming convention: `orgs/<org-slug>/arrangements/<arr-slug>/voices/<voice-slug>/annotations/<user-slug>/<name>.pdf` — alongside the *voice*, not a specific source-file format. Annotations are independent of source-file updates (this is what "survives format updates" in UC-15 means). The app surfaces a staleness indicator by comparing the annotation's mtime to the latest mtime of any file under the corresponding voice.
  **Visibility:** an annotation is **author-only-writable** but **org-readable** — any member of the owning org can read another member's annotations on that org's arrangements. The substitute-sees-the-regular's-bowings workflow is the motivating use case; strictly private notes belong in `/users/<slug>/library/` instead, not under an org's arrangement tree.
  **Audit:** personal annotation file events (create, replace, delete via WebDAV or REST) ARE captured in the audit log as `personal_annotation.<action>` entries keyed by `(voice_id, user_slug, name)`, even though no DB row backs them. The rule "every write to any persistent thing is logged" applies to MinIO objects under the annotations subtree too, not just DB rows.
- **Global annotations** are structured records in PostgreSQL — queryable, shared, authored by conductor/director.

#### Search & metadata
- **Search backend.** Postgres FTS (`tsvector` + GIN) over title, composer, arranger, instrumentation description, and tag names; `pg_trgm` for fuzzy matching on title and composer. No external search service — preserves the single-Postgres self-hostable footprint. If volume ever outgrows this, swap is mechanical.
- **Structured search fields.** Conductor's UC-2 dimensions split into structured columns vs. tags:
  - `Work.composer` (text, nullable) — composer lives on the abstract work, not the edition. Arrangements without a Work have no composer (folk, internal compositions).
  - `Arrangement.duration_seconds` (int, nullable) — range queries.
  - `Arrangement.difficulty` (smallint 1–8, nullable) — **ABRSM scale** is the canonical numeric for filters and ranges.
  - `Arrangement.difficulty_ratings` (jsonb, nullable) — optional map of scale → grade for cross-referencing, e.g. `{"abrsm": "6", "aba": "3.5", "henle": "5", "rcm": "8"}`. The `abrsm` key, if present, must agree with `difficulty` (rounded). UI suggests known scales (ABRSM, ABA, Henle, RCM, Trinity) via autocomplete.
  - `Arrangement.difficulty_notes` (text, nullable) — prose caveats (e.g. "grade 5 except the cadenza"); expected to be rarely used.
  - Instrumentation filter joins through `Voice.instrument_id`; no new field.

#### Authorization
- **Membership roles.** `Membership.role` has four values: `owner`, `archivist`, `conductor`, `musician` (stored as `text` + `CHECK` per the enum convention in Implementation conventions). Every org has at least one `owner`; demoting the last owner is rejected. The principal sub-role (`Membership.is_principal`) is **orthogonal to role** — it adds the section-coverage view without changing other permissions. Guests/substitutes don't need a Membership; `PartAssignment` implicitly grants the read access they need. See the Permission matrix subsection below for the per-role capability breakdown.

### Permission matrix

| Action | owner | archivist | conductor | musician |
|---|---|---|---|---|
| Manage members & roles | ✓ | | | |
| Org settings | ✓ | | | |
| Upload/edit arrangements & files | ✓ | ✓ | | |
| Manage tags | ✓ | ✓ | | |
| Build/edit collections | ✓ | ✓ | ✓ | |
| Global annotations | ✓ | | ✓ | |
| Part assignments | ✓ | ✓ | ✓ | |
| Read assigned parts | ✓ | ✓ | ✓ | ✓ |
| Personal annotations (own) | ✓ | ✓ | ✓ | ✓ |

### Entities

**Work** *(optional)*
The abstract musical work (e.g. "Beethoven: Symphony No. 5"). Lightweight, used for grouping and search. Not every arrangement requires one.
Fields: title, `composer` (text, nullable).

**Instrument**
An instance-wide controlled vocabulary entry. Seeded on first migration with the standard repertoire of ~150 orchestral/band/chamber instruments; admins can add more without a code change.
Fields: `key` (text, unique, machine-readable, stable — e.g. `trumpet_bb`), `display_name` (text, English default), `aliases` (text[], for search and import normalization), `family` (text — `brass`, `woodwind`, `strings`, `percussion`, `keyboard`, `voice`, `other`; drives UI grouping), `transposition` (text, nullable — e.g. `Bb`, `F`, `Eb`; null for non-transposing instruments), audit fields.
No `deleted_at` in phase 1: instruments referenced by Voice/Membership can't be hard-deleted; archival is a phase-2 concern.

**Arrangement**
A specific arrangement for specific instrumentation — what an organization licenses and owns. Also the unit of edition/variant: multiple editions of one work are multiple `Arrangement` rows under the same `Work`.
Fields: organization FK, title, `slug` (text, immutable, unique per organization), work (optional FK), instrumentation description, arranger, publisher, purchase date, license notes, copy count allowed, status (enum: `active`, `archived`), `duration_seconds` (int, nullable), `difficulty` (smallint 1–8, ABRSM, nullable), `difficulty_ratings` (jsonb, nullable), `difficulty_notes` (text, nullable), audit fields, `deleted_at`.

**Voice**
An individual instrument part within an arrangement (e.g. Flute 1, Violin II, Trumpet in Bb).
Fields: arrangement FK, name, `slug` (text, immutable, unique per arrangement), `instrument_id` FK to Instrument, audit fields, `deleted_at`.

**File**
An actual file representing a voice or full score, in a specific format.
Fields: voice FK (nullable for full scores), arrangement FK, `name` (text, filename without extension), `format` (enum: `lilypond`, `musicxml`, `pdf`, `image`), `mime_type` (text), `derived_from_file_id` (nullable FK to File), `conversion_quality` (enum: `clean`, `omr`, `manual`; null if not derived), audit fields, `deleted_at`.
Unique on `(arrangement_id, voice_id, name, format)`. The MinIO object key is **derived** from the entity tree (see Decisions on storage path), not stored — the WebDAV view and the REST view are guaranteed to agree.

**Organization**
An orchestra or ensemble.
Fields: `name` (text), `slug` (text, immutable, unique globally), audit fields.
No `deleted_at`: deleting an org is admin-gated hard delete because the cascade impact is large and undelete UX is messy.
**Cascade order** (FK-safe, one transaction): PartAssignments → CollectionItems → Collections → ArrangementTags → Tags → GlobalAnnotations → Files (and their MinIO objects) → Voices → Arrangements → Memberships → the Organization row. **Not cascaded:** `Work` rows (instance-wide, shared across orgs) and `audit_log` rows (retained with their `org_id` so the deletion itself stays forensically auditable after the org is gone).

**User**
A person using the system. No system-wide application role — authorization is per-org via `Membership.role`. Optional `is_system_admin` boolean for instance-level operations (org provisioning, user management).
Fields: `slug` (text, immutable, unique globally), `username` (citext, unique) — login identifier; renameable, distinct from `slug` so the personal WebDAV path stays stable across renames. `email` (citext, unique, nullable) — used for password reset and notifications; nullable so admins can create users without one (shared kiosk accounts, youth orchestra members). `password_hash` (text, nullable) — argon2id; nullable because OIDC users have no local password. `display_name` (text). `is_system_admin` (bool, default false). Audit fields.
No `deleted_at` initially — user deletion is admin-gated hard delete with explicit reassignment-or-anonymization of PartAssignments, GlobalAnnotations, and audit log entries. Phase 2 concern.

**AppPassword**
A revocable, WebDAV-scoped credential issued per device (the per-user app passwords from the Auth section).
Fields: user FK, `name` (text, user-visible label like "iPad in rehearsal room"), `hash` (text; argon2id of the random token — plaintext is never stored), `prefix` (text, 8 chars; first chars of the token in plaintext, used for log/UI identification without exposing the secret — the GitHub-PAT pattern), `created_at`, `last_used_at` (timestamptz, nullable; updated on each successful auth), `revoked_at` (timestamptz, nullable; null = active, non-null = revoked and never authenticates).
Unique on `(user, name)`. No `expires_at` in phase 1; nullable expiry column can be added later without a backfill (null = never expires).

Token format: `lied_<base64url(32 random bytes)>`. On creation the plaintext is shown to the user once and discarded server-side. WebDAV `Authorization: Basic base64(username:token)` resolves by:
1. Extracting the `prefix` (first 8 chars of the token) and looking up `WHERE user_id = ? AND prefix = ? AND revoked_at IS NULL` — typically returns a single candidate.
2. `argon2_verify(token, candidate.hash)` on the (rare collision aside) one row.

This keeps WebDAV auth fast despite argon2's intentional slowness — without the prefix lookup, every chatty WebDAV request would `argon2_verify` against every active app password the user has.

**Membership**
A user's membership in an organization.
Fields: user FK, organization FK, `role` (enum: `owner`, `archivist`, `conductor`, `musician` — see Decisions for the permission matrix), `instrument_ids` (int[] FK to Instrument), `is_principal` (bool, orthogonal to role), `principal_instrument_ids` (int[] FK to Instrument, subset of `instrument_ids` — array because principals may cover doublings, e.g. trumpet + flugelhorn).
Coverage check (UC-13): for an arrangement in a collection, every `Voice` whose `instrument_id ∈ principal_instrument_ids` must have a `PartAssignment`.
**FK integrity on the `int[]` columns:** Postgres can't enforce element-level FKs on an array, so `instrument_ids` / `principal_instrument_ids` are validated against the live `Instrument` set in the service layer on write. This is safe in phase 1 because `Instrument` has no soft-delete and rows are never hard-deleted while referenced — dangling IDs can only come from a bug, not normal flow. Flag for phase 2: if `Instrument` ever gains soft-delete, revisit (DB trigger, or normalize to a `membership_instruments` join table).
Invariant: every org has at least one Membership with `role = owner`; demoting the last owner is rejected.

**Collection**
A named, indexed set of arrangements belonging to an organization.
Fields: organization FK, name, `slug` (text, immutable, unique per organization), type (enum: `program`, `standing`), audit fields, `deleted_at`.
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
Instrument (instance-wide controlled vocabulary; seeded ~150 entries)

Work (optional, composer)
  └── Arrangement (status, duration, difficulty 1-8 ABRSM, difficulty_ratings, provenance)
        ├── File(s) [full score]
        ├── ArrangementTag → Tag
        └── Voice (instrument_id → Instrument)
              └── File (format, mime_type, derived_from?, conversion_quality?)
                    └── [personal annotation files in MinIO, by naming convention]

GlobalAnnotation → Arrangement

User
  └── AppPassword (revocable WebDAV credential)

Organization
  ├── Membership → User (role, instrument_ids[] → Instrument)
  ├── Tag (name, kind)
  └── Collection (program | standing, indexed by piece number)
        └── CollectionItem (index number) → Arrangement
              └── PartAssignment → User + Voice (notified_at, acknowledged_at)
```

---

## Out of scope (product boundary)

This is the product's outer boundary — distinct from the phase-2 *deferral* list below (those features are planned and the data model already accommodates them). The items here are things Lied deliberately does **not** try to be, possibly ever. Each has a mature tool that does it better; Lied integrates via open formats rather than competing.

**Notation & content creation**
- **Notation editing / engraving** — entering or editing the notes themselves. Use MuseScore, Frescobaldi (LilyPond), Sibelius, Finale, Dorico; Lied stores and serves their output.
- **Score authoring loop** — even the deferred conversion pipeline is format transformation, not interactive editing.
- **Arranging / orchestration / transcription tools** — generating new arrangements algorithmically.

**Audio & performance media**
- **Audio playback, MIDI synthesis, or playback of scores.**
- **Audio/video recordings as managed entities** — Lied is a sheet-music archive, not a media library, and has no first-class model for recordings. (A loose file may live in `/users/<user>/library/`, but it is not modeled.)
- **Practice tooling** — metronome, tuner, slow-down/loop trainers, page-turn pedals. These belong in a display client, which is itself deferred indefinitely.
- **Live performance sync** — networked page-turning across stands, conductor-driven page advance.

**Rights, commerce, and provenance beyond notes**
- **Rights/licensing beyond free-text fields** — Lied records `publisher`, `license notes`, `copy count allowed` as flat metadata for the archivist's reference. It does **not** enforce copy limits, track per-copy distribution for compliance, manage royalties, or model license terms structurally.
- **Purchasing / e-commerce** — UC-4 ("discover → purchase → archive") is a workflow pipeline *into* the archive, not a storefront. Lied never handles money or executes a purchase.
- **DRM / copy protection** on distributed files.

**People, scheduling, and ensemble operations**
- **Personnel & scheduling** — who sits in which seat, attendance, rehearsal calendars, availability. Only *voice/part distribution within a section* is in scope, not seating or rostering.
- **Payroll, dues, membership billing, communications/CRM.**
- **Concert/event management** — ticketing, venue booking, printed-handout programs. A Lied `Collection` is the *musical* program (pieces + parts), not the event.

**Infrastructure Lied relies on but does not provide**
- **A bundled display/reader app** — deferred indefinitely; musicians use forScore, MobileSheets, Xodo, or an OS file manager against WebDAV.
- **Identity provider** — Lied is an OIDC *client* (phase 2), never an IdP for other systems.
- **General cloud file sync** (Dropbox/Drive-style) — the WebDAV surface is scoped to the music tree, not arbitrary file storage. `/users/<user>/library/` is a convenience, not a sync product.
- **Email/SMS delivery infrastructure** — the deferred notification flow hands off to an external SMTP/provider; Lied runs no mail server.

**Tenancy posture** (cross-references NFR)
- **Strict multi-tenant SaaS isolation (topology 3)** — explicitly out for phase 1; requires a tenant-isolation pass. Phase 1 assumes every org in an instance trusts the others.

## Phase 1 cut

Smallest version that closes the **archivist → musician loop** end-to-end: an archivist uploads a piece, builds a concert program, assigns parts; each musician opens their part on an iPad via WebDAV. Later phases plug in cleanly because the data model already accommodates them.

### In scope for phase 1

**Backend infrastructure**
- Postgres + MinIO + REST API + WebDAV server.
- Local accounts only (argon2id); session cookies + bearer tokens for REST; app passwords for WebDAV.
- Rate limiting, shallow audit log, env / `_FILE` / `cmd:` secrets layer.

**Entities** — full data model from the spec: Organization, User, AppPassword, Membership, Work, Instrument, Arrangement, Voice, File, Collection, CollectionItem, PartAssignment, Tag, ArrangementTag, GlobalAnnotation. Even entities whose flows are deferred get their tables so the schema is stable.

**File handling**
- Upload/download via REST.
- Files stored and served as-is — **no conversion, no OMR**.
- All four formats (lilypond/musicxml/pdf/image) accepted; `format` and `mime_type` recorded but no pipeline runs on them.

**WebDAV**
- `/orgs/<org-slug>/arrangements/...` — read-write for archivists, read for assigned musicians.
- `/orgs/<org-slug>/collections/...` — read-only computed view.
- `/users/<user-slug>/library/` — private personal area.

**Search** — basic listing + ILIKE filter on `Arrangement.title` and `Work.composer` (via the optional Work join). No FTS, no `pg_trgm`, no tag-based search yet.

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

<!-- code-graph-mcp:begin v2 -->
## Code Graph (repo-wide AST index)

AST + FTS + vector index of the whole repo — prefer over multi-round Grep/Read for
structural queries (LSP only sees open files; this sees everything). Fastest path = Bash CLI:

| Intent | Command |
|--------|---------|
| Who calls X / what X calls | `code-graph-mcp callgraph X` |
| Impact before editing a fn | `code-graph-mcp impact X` |
| Unfamiliar dir / module | `code-graph-mcp overview <dir>` |
| Symbol source / signature | `code-graph-mcp show X` |
| Concept search (no exact name) | `code-graph-mcp search "…"` (vector: MCP `semantic_code_search`) |
| grep + AST context | `code-graph-mcp grep "pat" [paths]` |

Still use Grep for literal strings/regex in non-code files; still Read files you'll edit.
Full command + MCP-tool table: `.claude/plugin_code_graph_mcp.md`
<!-- code-graph-mcp:end -->
