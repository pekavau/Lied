# Lied — working on this repository

Lied is a self-hostable sheet-music archive and part-distribution tool for
orchestras and ensembles (see the [README](README.md)). **The design — personas,
use cases, architecture, API conventions, data model, non-functional posture
and the phase plan — lives in [`docs/design.md`](docs/design.md).** This file
holds only how to work on the code: the quality bar, conventions and
workflow every change follows.

Read the part of the design your change touches before you start:

| If you are changing… | read in `docs/design.md` |
|---|---|
| a `/v1` endpoint | *HTTP & REST API conventions* (and [`docs/api-guidelines.md`](docs/api-guidelines.md) for the how-to) |
| an entity, a migration, a query | *Data Model* (decisions, entities, unique constraints) |
| who may do what | *Auth & access* and the *Permission matrix* |
| WebDAV | *WebDAV layout* |
| a limit, config, secrets, audit | *Non-functional posture* |
| scope ("should we build X?") | *Phase cuts* and *Out of scope* |

Features marked *(not yet built)* or *(console only)* there are designed but
not exposed under `/v1`; check before relying on one.

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
- Other targets: `just fmt`, `just lint`, `just test`, `just openapi` (regenerates `docs/openapi.json`, the committed `/v1` contract), `just migrate`, `just prepare` (refreshes `.sqlx/` after SQL changes), `just dev` (`docker compose up` + `cargo watch`), `just build` (release binary), `just image` (docker image build), `just backup` (runs `pg_dump` + `mc mirror` against the docker-compose volumes).

### Toolchain pinning
- **`rust-toolchain.toml`** at the repo root pins the Rust version explicitly. Example:
  ```toml
  [toolchain]
  channel = "1.99.1"          # pin a specific release, never "stable"
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
- **REST resource identifier: UUID in `/v1` paths.** Endpoints address entities by UUID (`/v1/arrangements/{uuid}`), never by slug. Slugs are *scoped* (per-org / per-arrangement) and *renameable* (by an admin slug-rename op *(not yet built)*), so they're unsuitable as stable REST URLs; they remain the human-readable keys in the WebDAV tree only. A future slug lookup, if needed, is a `?slug=` resolver, not the canonical path.
- **JSON field casing: `camelCase`.** Wire JSON uses `camelCase`; Rust structs stay `snake_case` with `#[serde(rename_all = "camelCase")]` applied consistently. The `utoipa` schema reflects the same casing so the OpenAPI contract matches the wire.
- **Crate layout: workspace with a `lib` target.** The app is a workspace whose core is a library crate (`lied`) with a thin binary (`lied-server`) on top. Integration tests under `tests/` link the lib and may exercise internals directly, not only black-box HTTP — this is what the fixture builders (Testing posture) depend on.
- **Data access: SQL lives in `domain/*`.** Each `domain/<entity>.rs` owns that entity's struct and its `sqlx` queries, alongside the business rules that go with them; handlers (`routes/*`, the console, auth) call those functions and never write SQL, so the `/v1` tree, the console and the MCP server share one implementation. The one other home is `webdav/`, the filesystem view's permission-filtered access layer. No repository trait sits in front: queries are compile-time-checked and tests run against real Postgres, so a swappable interface would buy nothing. Pinned by `sql_lives_in_the_domain_layer` in `tests/layering.rs` (issue #85), whose named exception is the `/readyz` connection probe.
- **Enums: `text` + `CHECK`, not Postgres native enum types.** All status/type/kind columns (`Membership.role`, `Arrangement.status`, `Collection.type`, `File.format`, `File.conversion_quality`, …) are stored as `text` with a `CHECK (col IN (...))` constraint, mapped to Rust enums via sqlx. Native PG enum types were rejected: `ALTER TYPE ... ADD VALUE` is awkward (can't run in a transaction, can't remove values), and adding a variant should be a plain migration. `Tag.kind` stays free-text by design (no CHECK).
- **Wire names.** A path parameter is named after the collection segment before it, singular, plus `Id` (`orgs/{orgId}`, `members/{memberId}`, `app-passwords/{appPasswordId}`; never a bare `{id}`); since #66 these are the keys a bad parameter's Problem Details reports. A PATCH body is `Update<Resource>Request`, a create body `Create<Resource>Request`; the named exceptions are `AttachTagRequest` (links an existing tag) and the multipart `UploadForm`. Pinned by `path_parameters_are_named_after_their_collection` and `request_schemas_are_named_after_their_resource` (issue #74).
- **Validation: `garde` derive.** Input DTOs validate via `garde`; a single `garde::Report` → `AppError` mapping at the extractor boundary turns failures into a `400` Problem Details carrying a field-error extension member (`errors: { field: [messages] }` — RFC 7807 permits extensions). Uniform across every endpoint.

### Error handling
- **`thiserror`** for typed error enums in domain code (errors have shape).
- **`anyhow`** in the binary entrypoint and one-off scripts.
- One top-level `AppError` implements `axum::response::IntoResponse` so errors become HTTP responses consistently. For the `/v1` JSON tree it serializes as RFC 7807 Problem Details (`application/problem+json`); see *HTTP & REST API conventions* in `docs/design.md`.

### Logging
- `tracing` everywhere; never `println!` / `eprintln!` in production code.
- JSON output in production, pretty in dev (toggled via env var).
- Every request gets a request-ID span; logs use structured fields, never string interpolation.

### Testing posture
- **Unit tests** colocated with code; fast, no DB.
- **Integration tests** under `tests/`; use `testcontainers` for real Postgres + object storage (the pinned RustFS image in `tests/common`).
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
3. Migration applies cleanly from scratch, if schema changed; a new migration's checksum is appended to `lied/tests/migration_checksums.txt`, and no committed migration was edited (comments included — sqlx checksums the whole file and existing databases refuse to start, #111).
4. Public-API docs updated (rustdoc on public items) if behavior changed.
5. Branch rebased clean (no `fixup!` / `squash!` left).
6. `docs/design.md` updated if a design decision shifted (and CLAUDE.md if a working rule did).
7. If `/v1` behaviour changed: `docs/openapi.json` regenerated (`just openapi`; `openapi_snapshot_is_current` fails otherwise), and if an API convention changed, the test in `lied/tests/openapi.rs` that pins it updated or added. A crate version bump changes `info.version`, so it regenerates the snapshot too. A convention that only lives in prose drifts; one with a test does not.
8. Public docs checked: does the change affect any public `.md` file — README (features, quickstart, first steps, configuration, production and backup), CONTRIBUTING, SECURITY, `docs/`? If so, update it in the same PR; a stale README is how a new self-hoster first meets a bug.

### Documentation
- `rustdoc` on public items in any future library crates.
- `utoipa` generates OpenAPI from the routes; no separate hand-written API doc.
- `docs/design.md` is the design spec; CLAUDE.md is the working rules.
- README is the self-hoster's page: what Lied is, quickstart, first steps, configuration, running in production (incl. backup and restore). Design rationale stays here, not there.

### Security minima (beyond the security baseline in `docs/design.md`)
- SQL only via `sqlx::query!` / `sqlx::query_as!` (compile-time-checked, parameterized). String-formatted SQL is a PR blocker.
- XSS: `maud` auto-escapes; `PreEscaped` requires a justification comment.
- Secrets never logged: enforced by a single `Debug` impl on the config struct that redacts secret fields.

### Not in the bar
- No coverage threshold.
- No conventional-commits / changelog automation.
- No mandatory ADRs.
- No mutation / fuzz / property-based testing requirements (allowed, not required).

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
| grep + AST context | `code-graph-mcp grep "pat" [paths] [-t lang] [-g glob] [-c]` |

Still use Grep for literal strings/regex in non-code files; still Read files you'll edit.
Full command + MCP-tool table: `.claude/plugin_code_graph_mcp.md`
<!-- code-graph-mcp:end -->
