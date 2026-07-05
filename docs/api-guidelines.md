# Lied API Guidelines

How the `/v1` JSON REST API is shaped and documented. The OpenAPI spec
(`/openapi.json`, rendered at `/docs`) is the **customer-facing contract** —
it must be **consistent, comfortable, and complete**. This document is the
rule set that keeps it so; CLAUDE.md → "HTTP & REST API conventions" holds the
design rationale and must not be re-litigated here.

Resolves [issue #21]. Verified against `utoipa 5.5`, `utoipa-axum 0.1.3`.

---

## 1. Documentation mechanism — `utoipa-axum` `OpenApiRouter`

We use **`utoipa-axum`'s `OpenApiRouter` + the `routes!` macro**, *not* a
hand-maintained `#[openapi(paths(...))]` registry. The reason is mechanical
completeness: `routes!` registers a handler into the axum router **and** the
OpenAPI document in one call, so you cannot wire up a `/v1` endpoint without
also documenting it. A forgotten registry entry — the failure mode of the
manual approach — becomes impossible.

### Composition

Each resource module exposes `pub fn router() -> OpenApiRouter<AppState>`
instead of `axum::Router`. A single base `OpenApi` (info, tags, security
schemes, shared error components) seeds the tree; the modules merge/nest into
it; `split_for_parts()` yields the runnable `axum::Router` plus the finished
`OpenApi` to serve.

```rust
// lied/src/routes/openapi.rs  — the base document (info, tags, security, shared schemas)
#[derive(utoipa::OpenApi)]
#[openapi(
    info(title = "Lied API", version = env!("CARGO_PKG_VERSION")),
    modifiers(&SecurityAddon),
    tags(
        (name = "instruments",   description = "Instrument controlled vocabulary"),
        (name = "auth",          description = "Bearer-token minting"),
        (name = "app-passwords", description = "WebDAV credentials"),
        (name = "organizations", description = "Organizations"),
        (name = "users",         description = "User accounts"),
        (name = "members",       description = "Organization memberships"),
        (name = "works",         description = "Abstract musical works"),
        (name = "arrangements",  description = "Arrangements (editions)"),
        (name = "voices",        description = "Voices / parts"),
        (name = "tags",          description = "Per-org classifiers"),
        (name = "files",         description = "Score & voice files (MinIO-backed)"),
    ),
    components(schemas(ProblemDetails)),   // shared error body, registered once
)]
pub struct ApiDoc;

// lied/src/routes/v1.rs
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(version))
        .routes(routes!(list_instruments))
        .routes(routes!(mint_token))
        .routes(routes!(list_app_passwords, create_app_password))  // same path, two methods
        .routes(routes!(revoke_app_password))
        .merge(orgs::router())
        .merge(arrangements::router())
        .merge(files::router())
}

// where the /v1 tree is mounted (routes/mod.rs)
let (v1_router, mut api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
    .nest("/v1", v1::router())
    .split_for_parts();
// `api` is handed to the infra tree to serve at /openapi.json; `v1_router`
// gets the existing middleware stack (session layer, rate-limit, CSRF-exempt).
```

`routes!(a, b, …)` groups handlers that **share a URL path** (the path comes
from each handler's `#[utoipa::path(path = …)]`). List + create on the same
collection path go in one `routes!`; a nested sub-path gets its own.

`.route(path, method_router)` (no `s`) still exists for the rare handler we
deliberately leave **out** of the spec — none in `/v1` today.

### Mounting `/openapi.json` and `/docs`

Unchanged from today's infra tree: `/openapi.json` always serves the `OpenApi`
JSON; `/docs` renders RapiDoc and 404s when `LIED_DOCS_ENABLED=false`. The only
change is the source — the runtime-built `api` from `split_for_parts()` instead
of the empty `ApiDoc::openapi()` stub.

---

## 2. Annotation is mandatory and enforced

**Every `/v1` handler carries `#[utoipa::path(...)]`.** This is not best-effort.

- **Structural:** a handler with no `#[utoipa::path]` cannot be passed to
  `routes!` (it reads `path()`/`operation()`/`methods()` off the macro), so the
  ordinary way of adding a route already forces the annotation.
- **CI gate:** a test in `tests/openapi.rs` builds the composed spec (from the
  same `v1::api_router` production uses) and enforces the machine-checkable
  guidelines. Because axum's `Router` is not introspectable, the served route
  set is pinned two ways rather than compared to the router directly:
  1. the documented `OpenApi.paths` set must equal a reviewed `EXPECTED_PATHS`
     list **exactly** (both directions) — a new or renamed endpoint fails until
     the list is updated, a deliberate review checkpoint; and
  2. a source-lint forbids the bare `.route(` escape hatch anywhere in a `/v1`
     route module, so no endpoint can reach the router while skipping the
     `routes!`-driven OpenAPI registration.

  The test also asserts every operation has a non-empty `summary`, at least one
  `2xx` response, the `CommonErrors` triple on every authenticated operation,
  `412` on every `PATCH`, an RFC 7807 `ProblemDetails` body on every `4xx`/`5xx`
  response, and `camelCase` on every component-schema property.

Operation **summary** comes from the handler's first `///` doc-comment line;
the rest of the doc comment becomes the **description**. Write them for the API
consumer, terse but complete — no "TODO", no restating the function name.

---

## 3. Security schemes

Two schemes, both declared on the base `ApiDoc` via a `Modify` addon; `/v1`
accepts **either** a session cookie or a bearer token (per CLAUDE.md route-tree
boundary).

```rust
struct SecurityAddon;
impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let c = openapi.components.get_or_insert_with(Default::default);
        c.add_security_scheme("bearer", SecurityScheme::Http(
            HttpBuilder::new().scheme(HttpAuthScheme::Bearer).bearer_format("JWT").build()));
        c.add_security_scheme("session", SecurityScheme::ApiKey(
            ApiKey::Cookie(ApiKeyValue::new("id"))));  // tower-sessions cookie
    }
}
```

- **Default** for authenticated endpoints: `security(("bearer" = []), ("session" = []))`
  (an empty-scope list under two schemes = "either one satisfies"). Set this per
  operation; there is no usable global default that the per-path attribute can't
  override cleanly, so be explicit.
- **Unauthenticated** endpoints (`GET /v1/`, `POST /v1/tokens` — which *mints*
  the bearer from username/password) declare `security()` (empty) so the UI
  shows no lock and the contract says "no auth required".

---

## 4. Error model — complete and conformant per endpoint

The hard requirement: **every endpoint lists every status it can return, each
maps to a code the consumer expects, and the body is always RFC 7807 Problem
Details.** We get this without per-handler copy-paste by composing each
operation's `responses(...)` from small reusable `IntoResponses` profile types.

`responses(...)` accepts a comma-separated mix of inline `(status = …)` tuples
and bare `IntoResponses` type paths (verified in `utoipa-gen` 5.5
`path/response.rs`), so an operation is `<success tuple(s)> + <error profiles>`.

### 4.1 The canonical status contract

This table *is* "conform to expectations" — these meanings are fixed across the
whole API. All error bodies are `application/problem+json` (`ProblemDetails`).

| Status | When | `type` URI suffix |
|---|---|---|
| `400 Bad Request` | request body fails `garde` validation, or a bad `sort`/`filter`/pagination param | `validation-failed` |
| `401 Unauthorized` | no/invalid session or bearer | `unauthorized` |
| `403 Forbidden` | authenticated but role/ownership forbids the action | `forbidden` |
| `404 Not Found` | target absent, soft-deleted, or in a different org than the path | `not-found` |
| `409 Conflict` | unique-constraint clash, business invariant (last owner, still-referenced), format/ext mismatch | `conflict` |
| `412 Precondition Failed` | `If-Match` missing or stale on a mutation | `precondition-failed` |
| `413 Payload Too Large` | upload exceeds `LIED_MAX_UPLOAD_BYTES` | `payload-too-large` |
| `415 Unsupported Media Type` | upload MIME not in the format table | `unsupported-media-type` |
| `416 Range Not Satisfiable` | unsatisfiable `Range` on download | `range-not-satisfiable` |
| `429 Too Many Requests` | rate limit (login/token bucket, or per-identity ceiling) | `too-many-requests` |
| `500 Internal Server Error` | DB/storage/unexpected | `internal` |

`200`/`201`/`204`/`206` success codes are per endpoint (§5). These mirror the
existing `AppError` variants in `lied/src/error.rs` exactly — no new error
shapes, just documenting what already happens.

### 4.2 Profile types

Define once, in `lied/src/routes/openapi.rs`. Each derives `IntoResponses` and
references the shared `ProblemDetails` schema. Granular types compose to match
each endpoint class precisely (no over-claiming a code an endpoint can't emit).

```rust
/// 401 + 429 + 500 — every authenticated endpoint can emit these.
#[derive(utoipa::IntoResponses)]
pub enum CommonErrors {
    #[response(status = 401, description = "Authentication required")]
    Unauthorized(#[to_schema] ProblemDetails),
    #[response(status = 429, description = "Rate limit exceeded")]
    TooManyRequests(ProblemDetails),
    #[response(status = 500, description = "Internal error")]
    Internal(ProblemDetails),
}
// Atomic profiles, same shape:
//   Forbidden403, NotFound404, Validation400, Conflict409,
//   Precondition412, PayloadTooLarge413, UnsupportedMedia415, Range416
```

(Exact `IntoResponses` derive spelling — `#[to_schema]`, body binding — to be
confirmed at first compile; the principle is one type per code, body =
`ProblemDetails`.)

### 4.3 Per-class composition

| Endpoint class | Success | Error profiles |
|---|---|---|
| **GET list** (`/orgs`, `…/voices`, …) | `200 Page<T>` | `CommonErrors`, `Forbidden403`, `Validation400`¹ |
| **GET by id** | `200 T` + `ETag` | `CommonErrors`, `Forbidden403`, `NotFound404` |
| **POST create** | `201 T` + `ETag` | `CommonErrors`, `Forbidden403`, `Validation400`, `Conflict409`, `NotFound404`² |
| **PATCH update** | `200 T` + `ETag` | `CommonErrors`, `Forbidden403`, `NotFound404`, `Validation400`, `Conflict409`, `Precondition412` |
| **DELETE** (soft) | `204` | `CommonErrors`, `Forbidden403`, `NotFound404`, `Conflict409`³, `Precondition412` |
| **POST `…/undelete`** | `204` | `CommonErrors`, `Forbidden403`, `NotFound404` |
| **POST attach** (`…/tags`) | `201` | `CommonErrors`, `Forbidden403`, `NotFound404`, `Conflict409` |
| **DELETE detach** | `204` | `CommonErrors`, `Forbidden403`, `NotFound404` |
| **POST upload** (files) | `201 FileResponse` | create set **+** `PayloadTooLarge413`, `UnsupportedMedia415` |
| **PUT replace** (files) | `200 FileResponse` | update set **+** `PayloadTooLarge413`, `UnsupportedMedia415` |
| **GET download** (files) | `200` / `206` octet-stream | `CommonErrors`, `Forbidden403`, `NotFound404`, `Range416` |

¹ list endpoints `400` on an unknown `sort`/`filter` field or bad pagination.
² `NotFound404` only when the create is nested (parent org/arrangement/voice must exist).
³ `409` on delete only where a live reference blocks it (e.g. a `Work` with live arrangements).

---

## 5. Schema & resource conventions

Most of this is already in CLAUDE.md; recorded here as the OpenAPI encoding.

- **Casing.** Wire JSON is `camelCase`; structs are `snake_case` +
  `#[serde(rename_all = "camelCase")]`; `ToSchema` reflects the same. Response
  DTOs and `Page<T>` already follow this.
- **Pagination envelope.** `Page<T>` is a generic `#[derive(ToSchema)]` with a
  `T: ToSchema` bound. List operations reference it inline —
  `body = inline(Page<ArrangementResponse>)` — which materializes the concrete
  schema at the use site with no separate alias bookkeeping. (utoipa also
  supports named `#[aliases(...)]` for generics; `inline` is preferred here so
  the item type lives next to the operation and there is no alias table to keep
  in sync.)
- **ETag / If-Match.**
  - GET-by-id and mutation success responses document an `ETag` **response
    header** (`responses((status = 200, body = T, headers(("ETag" = String, description = "updated_at ms-epoch, quoted")))))`).
  - Mutations document `If-Match` as a **required request header param**:
    `params(("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"))`.
- **Query params.** `limit`, `offset`, `sort`, `filter[…]`, `q` are documented
  `params(... Query ...)`; the per-resource sort/filter **allowlist** goes in the
  param description (the allowlists live in `lied/src/listing.rs`). Default sort
  is stated. Bracket `filter[field]` syntax matches CLAUDE.md.
- **Path ids** are UUIDs (`("org_id" = Uuid, Path, …)`), never slugs — slugs are
  WebDAV-only.
- **Resource shape** (already established, documented as-is): nested collections
  (`…/arrangements/{id}/voices`, `…/voices/{id}/files`); state-change
  sub-resources are `POST …/undelete`; **PUT = full file replacement** (new row +
  soft-delete old), **PATCH = partial metadata update**. Don't invent new verbs.
- **Multipart upload.** OpenAPI can't model the stream directly; use the marker
  schema `UploadForm { #[schema(value_type = String, format = Binary)] file }`
  (`#[allow(dead_code)]`, exists only so the UI shows a file picker) with
  `request_body(content = UploadForm, content_type = "multipart/form-data")`.

---

## 6. Retrofit plan (this PR)

Per #21, **all 67 endpoints** are annotated in this PR so `/openapi.json` is
complete on merge. Order, lowest-risk first:

1. **Infrastructure:** `routes/openapi.rs` — `ApiDoc` base, `SecurityAddon`,
   `ProblemDetails` `ToSchema`, the `IntoResponses` profile types, `Page<T>`
   aliases. Switch each `routes/*.rs` `router()` to `OpenApiRouter`; rewire
   `routes/mod.rs` to `with_openapi(...).split_for_parts()`.
2. **Annotate, by module:** `v1.rs` (instruments, tokens, app-passwords),
   `orgs.rs` (orgs/users/members), `arrangements.rs` (works/arrangements/voices/
   tags), `files.rs` (score/voice files). The `wip-openapi-file-docs` branch has
   working `#[utoipa::path]` examples for the file + token endpoints to adapt.
3. **CI gate:** `tests/openapi.rs` (§2) — route-set equality + per-operation
   summary/response checks.
4. **Verify:** `just check`; load `/docs`, confirm every group appears, error
   responses render, "Authorize" accepts a bearer token; `/openapi.json`
   validates.

Existing response DTOs already deriving `ToSchema` (`OrganizationResponse`,
`MembershipResponse`, `ArrangementResponse`, `VoiceResponse`, `TagResponse`,
`FileResponse`) are reused; add `ToSchema` to the remaining request/response
DTOs and to `Page<T>`.

---

## 7. Future consumer: MCP server (phase 2)

The OpenAPI spec's completeness pays off twice: a phase-2 **MCP server** (see
CLAUDE.md → Architecture → "MCP server (deferred — phase 2)") is a thin adapter
over this same `/v1` surface and the domain/service layer. A complete, accurate
spec is the source these MCP tools are generated/derived from. Two standing
implications for `/v1` work, both already true and just reaffirmed here:

- keep the spec **complete and machine-consumable** (the whole point of §2); and
- keep handlers thin — **no business logic an MCP tool couldn't reach without
  going through HTTP** (logic stays in `domain/*`, as it does now).

No phase-1 code is required for MCP.

[issue #21]: https://github.com/pekavau/Lied/issues/21
