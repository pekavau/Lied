# Lied

**Self-hostable sheet music management for orchestras and ensembles.**

[![CI](https://github.com/pekavau/Lied/actions/workflows/ci.yml/badge.svg)](https://github.com/pekavau/Lied/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Lied is a music library and part-distribution tool for the people who keep an
ensemble's repertoire organized: the **archivist** who maintains the archive
and assembles concert programs, the **conductor** who plans them, and the
**musicians** who need the right part, for the right instrument, in the right
context. It stores scores and parts, tracks provenance and instrumentation,
builds indexed collections (concert programs and standing repertoire), assigns
parts to players, and serves everything over open protocols so musicians can
open their music in the reader app they already use.

It is designed to be **run by the ensemble that uses it** — a single container,
Postgres, and object storage, with no mandatory cloud dependency.

> **Status:** early and under active development. The core archivist → musician
> workflow works end-to-end, as do the desktop management console and archive
> search (see [Roadmap](#roadmap)). There is no dedicated musician
> app yet — musicians use existing WebDAV-capable readers. Expect breaking
> changes before a 1.0.

## Why Lied

An ensemble's music lives in many places — publisher PDFs, LilyPond and
MusicXML sources, scanned parts, personal copies with bowings and fingerings —
and getting the right one to the right stand is a recurring chore. Existing
tools engrave notation or display PDFs very well; few help an archivist *manage*
and *distribute* a repertoire. Lied fills that gap and deliberately does **not**
try to be a notation editor, a display/reader app, or an events/personnel
system — it integrates with the tools that already do those well, via open
formats.

Two design commitments shape everything:

- **Self-hostability is a hard requirement.** No feature depends on a cloud
  service you don't control.
- **Open protocols over proprietary ones.** Your music stays accessible even
  without this app installed.

## Features

- **Archive & catalog** — arrangements with provenance (publisher, arranger,
  purchase, license notes), instrumentation, difficulty, and duration; grouped
  under abstract *works*; classified with per-org tags. Multiple editions of a
  work coexist.
- **Formats** — LilyPond, MusicXML, PDF, and images (scanned/photographed
  parts) are all first-class. The format of every file is surfaced; images are
  accepted as display-only without forced conversion.
- **Collections** — indexed concert programs (fixed sequence) and standing
  repertoire (drawn from by number), assembled from the archive.
- **Part distribution** — assign a voice in a program to a specific player;
  guests and substitutes get read access to their parts without joining the
  organization.
- **Two complementary interfaces over one backend:**
  - a **WebDAV** filesystem view of the music tree — mount it in your OS or open
    it from a tablet reader, MuseScore, or Frescobaldi; and
  - a **REST API** (`/v1`) plus a server-rendered **admin UI** for the
    music-aware operations WebDAV has no concept of.
- **Auth built for self-hosting** — local accounts (argon2id), session cookies
  and bearer JWTs for the API, and revocable per-device app passwords for
  WebDAV. Permissions are scoped per organization by role.
- **Operability from day one** — rate limiting, an audit log of every write,
  a layered secrets story (`env` / `_FILE` / external command), optional
  Prometheus metrics, and an OpenAPI spec served from the running binary.

## Architecture

```
Lied (Rust + axum)              ← serves every interface from one binary
    ├── WebDAV                  ← open access: OS mounts, tablet readers,
    │                             MuseScore, Frescobaldi, any WebDAV client
    └── REST API + admin UI     ← music-aware: search, collections, part
        │                         assignment, annotations
        ├── PostgreSQL          ← structured metadata + audit log
        └── MinIO (S3-compatible) ← file storage
```

WebDAV and REST are complementary: WebDAV is a filesystem view of the files;
REST is the music-aware layer on top. MinIO speaks the S3 API, so it can be
swapped for any S3-compatible store without code changes.

The full design rationale — data model, authorization, non-functional posture,
and the phased delivery plan — lives in [`docs/design.md`](docs/design.md), which is the
project's design spec, including the HTTP/API conventions. How to add or
change a `/v1` endpoint is in [`docs/api-guidelines.md`](docs/api-guidelines.md).

### Tech stack

Rust · [axum](https://github.com/tokio-rs/axum) · [sqlx](https://github.com/launchbadge/sqlx)
(compile-time-checked SQL, no ORM) · PostgreSQL · MinIO ·
[dav-server](https://crates.io/crates/dav-server) · HTMX + [maud](https://maud.lang.rs/)
for the admin UI · [utoipa](https://github.com/juhaku/utoipa) for OpenAPI ·
packaged as a distroless Docker image.

## Quickstart

**Prerequisites:** Docker + Docker Compose.

```bash
git clone https://github.com/pekavau/Lied.git
cd Lied
cp .env.example .env        # adjust secrets before any non-local use
docker compose up
```

This starts Postgres, MinIO, and the app; on startup the app applies any
pending schema migrations and creates its storage bucket if it is missing.
Then create the first system administrator (you'll be prompted for a password if you
omit `--password`):

```bash
docker compose run --rm app \
  cargo run --package lied-server -- \
  create-admin --username admin --display-name "Admin" --email admin@example.org
```

(The dev image runs via `cargo watch`; the release image exposes `lied-server`
as its entrypoint, so a production deployment runs `lied-server create-admin …`
directly.)

Once it's up:

| Interface | URL |
|---|---|
| Admin UI | `http://localhost:8080/admin` |
| REST API | `http://localhost:8080/v1` |
| API docs (RapiDoc) | `http://localhost:8080/docs` |
| OpenAPI spec | `http://localhost:8080/openapi.json` |
| WebDAV (org tree) | `http://localhost:8080/orgs/<org-slug>/…` |
| WebDAV (personal) | `http://localhost:8080/users/<user-slug>/library/` |
| MinIO console | `http://localhost:9001` |

Musicians authenticate to WebDAV with a per-device **app password**, which
any logged-in user creates on the admin UI's **App passwords** page
(`/admin/app-passwords`), then mount the tree in their OS file manager or open
it from a reader such as forScore, MobileSheets, or MuseScore.

## First steps

From an empty instance to a musician holding their part:

1. **Log in** at `/admin` as the system administrator you just created.
2. **Create an organization** under *Organizations*, and **create users** for
   its people under *Users* (only a system administrator creates accounts).
3. **Open the workspace** from the home page and add the users as members
   under *Members*, each with a role — `owner`, `archivist`, `conductor` or
   `musician` — and, for musicians, the instruments they play.
4. As an archivist, **add an arrangement** with its voices and **upload** the
   score and part files under *Arrangements*.
5. **Build a collection** — a concert program or a standing book — under
   *Collections*, then **assign parts** to players. A player may be a guest
   with no membership; the assignment alone lets them read that part.
6. Each musician logs in once, creates an **app password** on the
   *App passwords* page, and points their reader app at
   `http://<host>:8080/orgs/<org-slug>/` with their username and that app
   password. They see the arrangements they hold parts in.

> Serving over HTTPS? Set `LIED_SECURE_COOKIES=true` — browsers drop `Secure`
> cookies on plain-HTTP origins, so it defaults to `false` for local/LAN use.

## Configuration

All configuration is via environment variables (each secret `X` also accepts
`X_FILE` pointing at a file, the Docker-secrets convention). The most common:

| Variable | Default | Purpose |
|---|---|---|
| `LIED_BIND_ADDR` | `0.0.0.0:8080` | Listen address |
| `LIED_DATABASE_URL` | — | Postgres connection string |
| `LIED_S3_ENDPOINT` | `http://localhost:9000` | S3-compatible endpoint (MinIO) |
| `LIED_S3_BUCKET` | `lied` | Bucket for stored files; created on startup if missing |
| `LIED_S3_ACCESS_KEY_ID` / `LIED_S3_SECRET_ACCESS_KEY` | — | Object-storage credentials |
| `LIED_JWT_SIGNING_KEY` | — | HS256 secret for bearer tokens (use `_FILE`/`_CMD` in prod) |
| `LIED_SECURE_COOKIES` | `false` | Set `true` behind HTTPS |
| `LIED_MAX_UPLOAD_BYTES` | `200 MB` | Single-file upload ceiling |
| `LIED_METRICS_ENABLED` | `false` | Expose unauthenticated `/metrics` |
| `LIED_DOCS_ENABLED` | `true` | Serve the `/docs` UI |
| `LIED_RATELIMIT_LOGIN_PER_MIN` | `10` | Brute-force defense on login |

See [`.env.example`](.env.example) for a starting point and the **Default
limits** table in [`docs/design.md`](docs/design.md) for the full list — every limit is
documented and configurable.

## Running in production

The release image (`just image`, or `docker build .`) is a distroless image
whose entrypoint is `lied-server`; run it next to Postgres and any
S3-compatible store.

- **Schema and bucket** are handled on startup: pending migrations are applied
  before the server listens (safe with several instances starting at once),
  and the bucket is created if missing. An instance that finds a migration it
  doesn't know — an older image against a newer database — refuses to start.
- **Secrets** (`LIED_DATABASE_URL`, `LIED_JWT_SIGNING_KEY`, the S3 keys) should
  not sit in plain environment variables: each also reads `<NAME>_FILE` (a
  path, the Docker/Kubernetes secrets convention) or `<NAME>_CMD` (a command
  whose output is the secret, e.g. `pass`, `op read`, `vault kv get`). The
  signing key should be 32 random bytes.
- **HTTPS** belongs in a reverse proxy in front of Lied; set
  `LIED_SECURE_COOKIES=true` once it is in place.
- **First administrator:** `lied-server create-admin --username … --display-name … --email …`.
- `/healthz` (process up) and `/readyz` (Postgres and storage reachable) are
  for your orchestrator; `/metrics` is off unless `LIED_METRICS_ENABLED=true`.

### Backup and restore

Postgres is the authority on what exists; object storage holds the bytes. Back
up **Postgres first, then storage**, and restore in the same order: a database
row whose file was not yet mirrored is a detectable 404, while the reverse
would leave silent dead bytes.

With the compose stack, `just backup` does both into `./backup/`:
`postgres-<timestamp>.dump` (`pg_dump --format=custom`) and
`minio-<timestamp>/` (`mc mirror` of the MinIO server). Run it from cron. To restore,
stop the app, then:

```bash
docker compose exec -T postgres pg_restore --clean --if-exists \
  -U lied -d lied < backup/postgres-<timestamp>.dump
# then mirror backup/minio-<timestamp>/ back into the bucket with `mc mirror`
```

Back up the `.env` / secrets files too. Sessions and rate-limit state are
not worth keeping: after a restore users log in again. This is best effort,
not a consistent snapshot: there is no point-in-time recovery, WAL streaming or
restore verification yet.

## Development

[`just`](https://github.com/casey/just) is the task runner; the toolchain is
pinned in `rust-toolchain.toml`.

```bash
just dev       # docker compose up + hot reload
just check     # fmt + clippy + test — the canonical verification
just migrate   # apply migrations to $DATABASE_URL
just prepare   # refresh the .sqlx offline query cache after SQL changes
```

Every pull request must pass the CI gates: `fmt --check`, `clippy -D warnings`,
`test --all`, `cargo deny check`, `sqlx prepare --check`, and a Docker image
build. Integration tests run against ephemeral Postgres + MinIO via
`testcontainers`, so a working Docker daemon is required to run the full suite.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the workflow and the quality bar.

## Roadmap

Lied is built in phases; the data model already accommodates the later ones.

- **Phase 1 — complete.** The archivist → musician loop end-to-end: upload a
  piece, build a concert program, assign parts, and have each musician open
  their part over WebDAV. Full schema, local auth, REST API, WebDAV, and a
  minimal admin UI.
- **Phase 2 — complete.** A feature-complete management/curation console for
  desktop personas, plus real archive search (full-text + fuzzy), the
  section-coverage check, and global-annotation authoring.
- **Phase 3+ — planned.** A dedicated cross-platform musician/reader client;
  format conversion pipelines (LilyPond ⇄ MusicXML → PDF) and OMR; automated
  distribution (email/sync); OIDC/SSO; and an MCP server over the API.

Explicitly **out of scope** (integrate, don't reinvent): notation editing,
audio/MIDI playback, rights/royalty enforcement, purchasing, and
personnel/scheduling. See the *Out of scope* section in `docs/design.md`.

## Contributing & security

- Contributions are welcome — start with [`CONTRIBUTING.md`](CONTRIBUTING.md).
- To report a vulnerability, see [`SECURITY.md`](SECURITY.md) (please don't open
  a public issue for security problems).
- Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in Lied by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
