# Contributing to Lied

Thanks for your interest in Lied! This is an early-stage project, so the most
useful contributions right now are bug reports, focused fixes, and feedback from
anyone self-hosting it. Larger features are best discussed in an issue first —
the design is deliberately phased (see the roadmap in the [README](README.md)
and the full rationale in [`docs/design.md`](docs/design.md)), and it helps to check that a
change fits the current phase before you build it.

## Getting set up

**Prerequisites:** Docker + Docker Compose, and the Rust toolchain pinned in
`rust-toolchain.toml` (rustup installs it automatically on first build).
[`just`](https://github.com/casey/just) is the task runner.

```bash
git clone https://github.com/pekavau/Lied.git
cd Lied
cp .env.example .env
just dev          # Postgres + RustFS + app with hot reload
```

The integration tests spin up ephemeral Postgres + RustFS with
[`testcontainers`](https://github.com/testcontainers/testcontainers-rs), so a
running Docker daemon is required to run the full suite. For compile-time SQL
checking you'll want a database reachable at `DATABASE_URL`
(`postgres://lied:lied@localhost:5432/lied` with the dev compose stack).

## The quality bar

Before opening a pull request, run the canonical check:

```bash
just check        # fmt + clippy + test
```

CI enforces these gates on every PR, and all must pass before merge:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all`
- `cargo deny check` — dependency license + advisory policy (`deny.toml`)
- `cargo sqlx prepare --check` — the committed `.sqlx/` offline cache is in sync
- the Docker image builds

A few project-specific rules worth knowing up front:

- **SQL goes through `sqlx::query!` / `query_as!`** (compile-time-checked,
  parameterized). String-formatted SQL is not accepted. After changing any SQL,
  run `just prepare` and commit the updated `.sqlx/` cache.
- **A committed migration is never edited, not even a comment.** sqlx
  checksums every applied migration and the server refuses to start when one
  changed. Fix a schema mistake in a new migration, and append the new one's
  checksum to `lied/tests/migration_checksums.txt` (the
  `applied_migrations_are_never_edited` test prints the line).
- **Business logic lives in `domain/*`**, not in HTTP handlers — the REST and
  (future) MCP surfaces call the same service functions.
- `#![forbid(unsafe_code)]`; `unwrap()`/`expect()` in production paths trip
  clippy (tests may use them freely).
- New behavior should come with a test. Coverage isn't gated on a percentage —
  meaningful tests over numbers.
- The API conventions (pagination, ETags/optimistic concurrency, RFC 7807 error
  shape) are in `docs/design.md` ("HTTP & REST API conventions"); how to add or
  change a `/v1` endpoint, including its OpenAPI annotation, is in
  [`docs/api-guidelines.md`](docs/api-guidelines.md).
- `docs/openapi.json` is the committed `/v1` contract. A change that alters the
  API must regenerate it with `just openapi` (CI fails otherwise), so the
  contract change is visible as a diff in review. If you change an API
  convention, change the test in `lied/tests/openapi.rs` that pins it too.
- If your change affects what a public `.md` file says — the README's
  quickstart, first steps, configuration or production notes, this file,
  `SECURITY.md`, or `docs/` — update it in the same PR.

## Pull request workflow

- One issue → one branch → one PR. Branch off `main`.
- `main` stays **linear** — the repo merges via *rebase and merge*, no merge
  commits. Keep your branch rebased on `main` and squash away any
  `fixup!` / `squash!` commits before requesting review
  (`git rebase -i --autosquash main`).
- Reference the issue in the PR and include a short checklist of what you
  changed. If a design decision shifted, update `docs/design.md` in the same PR.
- Commit messages are free-form and imperative; no conventional-commits
  enforcement.

## Reporting bugs

Open a GitHub issue with steps to reproduce, what you expected, and what
happened (logs help). If it's a **security** issue, please **do not** open a
public issue — follow [`SECURITY.md`](SECURITY.md) instead.

## License of contributions

By contributing, you agree that your contributions will be dual-licensed under
`MIT OR Apache-2.0`, matching the project (see the [README](README.md#license)),
without any additional terms.
