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
> workflow works end-to-end (see [Roadmap](#roadmap)); the desktop management
> console and richer search are being built now. There is no dedicated musician
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
  guests and substitutes get access without a full account.
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
and the phased delivery plan — lives in [`CLAUDE.md`](CLAUDE.md), which is the
project's design spec. The HTTP/API conventions are in
[`docs/api-guidelines.md`](docs/api-guidelines.md).

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

This starts Postgres, MinIO, and the app with schema migrations applied. Then
create the first system administrator (you'll be prompted for a password if you
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

Musicians authenticate to WebDAV with a per-device **app password** (created in
the admin UI), then mount the tree in their OS file manager or open it from a
reader such as forScore, MobileSheets, or MuseScore.

> Serving over HTTPS? Set `LIED_SECURE_COOKIES=true` — browsers drop `Secure`
> cookies on plain-HTTP origins, so it defaults to `false` for local/LAN use.

## Configuration

All configuration is via environment variables (each secret `X` also accepts
`X_FILE` pointing at a file, the Docker-secrets convention). The most common:

| Variable | Default | Purpose |
|---|---|---|
| `LIED_DATABASE_URL` | — | Postgres connection string |
| `LIED_S3_BUCKET` / `LIED_S3_ACCESS_KEY_ID` / `LIED_S3_SECRET_ACCESS_KEY` | — | Object storage |
| `LIED_JWT_SIGNING_KEY` | — | HS256 secret for bearer tokens (use `_FILE`/`_CMD` in prod) |
| `LIED_SECURE_COOKIES` | `false` | Set `true` behind HTTPS |
| `LIED_MAX_UPLOAD_BYTES` | `200 MB` | Single-file upload ceiling |
| `LIED_METRICS_ENABLED` | `false` | Expose unauthenticated `/metrics` |
| `LIED_DOCS_ENABLED` | `true` | Serve the `/docs` UI |
| `LIED_RATELIMIT_LOGIN_PER_MIN` | `10` | Brute-force defense on login |

See [`.env.example`](.env.example) for a starting point and the **Default
limits** table in [`CLAUDE.md`](CLAUDE.md) for the full list — every limit is
documented and configurable.

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
- **Phase 2 — in progress.** A feature-complete management/curation console for
  desktop personas, plus real archive search (full-text + fuzzy), the
  section-coverage check, and global-annotation authoring.
- **Phase 3+ — planned.** A dedicated cross-platform musician/reader client;
  format conversion pipelines (LilyPond ⇄ MusicXML → PDF) and OMR; automated
  distribution (email/sync); OIDC/SSO; and an MCP server over the API.

Explicitly **out of scope** (integrate, don't reinvent): notation editing,
audio/MIDI playback, rights/royalty enforcement, purchasing, and
personnel/scheduling. See the *Out of scope* section in `CLAUDE.md`.

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
