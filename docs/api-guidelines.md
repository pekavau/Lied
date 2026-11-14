# Adding or changing a `/v1` endpoint

The *rules* of the `/v1` contract — pagination, ETags, Problem Details,
sort/filter allowlists, UUID paths, `camelCase` — live in CLAUDE.md
("HTTP & REST API conventions" and "Implementation conventions"). This page is
only the *how*: which pieces of the codebase to reach for so a new endpoint
follows those rules. The OpenAPI spec (`/openapi.json`, rendered at `/docs`)
is the published contract and is built from the code, so the code is the
reference for anything this page does not say.

**The checklist is `lied/tests/openapi.rs`.** It builds the spec from the same
`v1::api_router` production serves and fails on anything below that can be
checked mechanically. Test names are quoted where a rule is pinned by one; read
the test rather than a restatement here.

## 1. Register the route through `routes!`

Each module under `lied/src/routes/` returns an `OpenApiRouter<AppState>` and
registers handlers with `utoipa_axum::routes!`, which adds the handler to axum
and to the OpenAPI document in one call. Handlers that share a URL path go in
one `routes!(…)`; the module's router is merged into `v1::api_router`.

- A handler without `#[utoipa::path]` cannot be passed to `routes!`.
- A bare `.route(…)` in a `/v1` module is refused
  (`no_v1_route_bypasses_openapi_registration`).
- A new or renamed path must be added to `EXPECTED_PATHS`
  (`path_set_matches_expected_exactly`). That edit is the review checkpoint.
- A new resource group gets its tag declared on `ApiDoc` in
  `lied/src/routes/openapi.rs`.

## 2. Annotate with `#[utoipa::path]`

- **Summary and description.** Set `summary` (non-empty:
  `every_operation_has_a_non_empty_summary`) and, where it helps, a
  `description`, written for the API consumer.
- **Parameters.** Path parameters are named as on the wire (`orgId`) and typed
  `Uuid` (`every_path_parameter_is_a_uuid`). Mutations guarded by `If-Match`
  declare it as a `Header` parameter.
- **Security.** Authenticated operations declare
  `security(("bearer" = []), ("session" = []))`. Only the operations listed in
  `security_requirements_declare_bearer_or_session_or_none` may also allow
  anonymous callers; an empty `()` entry is what marks that.
- **Success responses.** At least one `2xx`
  (`every_operation_has_at_least_one_2xx_response`). Lists return
  `body = inline(Page<T>)`; single-entity reads and writes declare the `ETag`
  response header. File uploads use `request_body(content = UploadForm,
  content_type = "multipart/form-data")` (`lied/src/routes/files.rs`).
- **Error responses.** Compose them from the profile types in
  `lied/src/routes/openapi.rs`, listing only what the handler can actually
  return:

  | Profile | Statuses | Use when |
  |---|---|---|
  | `CommonErrors` | 401, 429, 500 | every authenticated operation (`authenticated_operations_declare_common_errors`) |
  | `Validation400` | 400 | any path/query parameter or body (`operations_with_parameters_declare_400`) |
  | `JsonBodyErrors` | 413, 415 | any JSON body, together with `Validation400` (`operations_with_a_json_body_declare_its_rejections`) |
  | `PayloadTooLarge413`, `UnsupportedMedia415` | 413, 415 | multipart uploads |
  | `Forbidden403` | 403 | a role or ownership check |
  | `NotFound404` | 404 | an addressed entity or parent that may be absent |
  | `Conflict409` | 409 | a unique constraint or business invariant |
  | `Precondition412` | 412 | an `If-Match`-guarded write (required on every `PATCH`: `patch_operations_declare_precondition_412`) |
  | `Range416` | 416 | a `Range`-honouring download |

  Every error body is `ProblemDetails`
  (`error_responses_use_problem_details_schema`); the profiles already do this.
- **Schemas.** DTOs derive `ToSchema` with `#[serde(rename_all = "camelCase")]`
  (`schema_properties_are_camel_case`).

## 3. Extract with the Problem Details wrappers

axum's own extractors reject a bad request in plain text. In `/v1` use:

- `routes::json::ApiJson<T>` for JSON bodies;
- `routes::params::{ApiPath, ApiQuery}` for path and query parameters,
  conventionally imported as `ApiPath as Path, ApiQuery as Query`. `clippy.toml`
  forbids `axum::extract::{Path, Query}`, so CI catches a slip.

## 4. ETags and guarded writes

Helpers in `lied/src/listing.rs`:

- `etag_response` / `etag_response_status` — a JSON response carrying the
  entity's `ETag`.
- `if_match_header` + `check_if_match` — the read-time `If-Match` check; map
  its error to `AppError::PreconditionFailed`.
- `version_of` — the version to hand the domain write.
- `stale_or_gone` — when the guarded write matches no row, re-read the entity
  to answer `412` (it is still there) or `404` (it is gone).

The domain write itself re-checks the version in its SQL, so two writers
holding one ETag cannot both succeed. The contract for those functions is the
"Guarded writes" section of the module docs in `lied/src/domain/mod.rs`;
`update_collection` in `lied/src/routes/collections.rs` is a complete example
of the handler side.

## 5. `PATCH` bodies

`PATCH` is a partial update (CLAUDE.md, issue #23). Give it its own body type,
separate from the create body, with `deny_unknown_fields` and every field
optional (`patch_request_bodies_have_no_required_fields`). The helpers are in
`lied/src/routes/patch.rs` (module docs state the rules): `present_or_null` to
tell an omitted nullable field from an explicit `null`, `reject_blank` for
required text fields, and `changed_fields` for the audit diff. A body that
changes nothing returns the current entity without writing.

## 6. Keep logic in `domain/*`

Handlers authorize, extract, call a `domain/*` function, audit and respond.
Business rules belong in `domain/*` so the console and any later consumer
(CLAUDE.md, "MCP server") call the same functions instead of going through
HTTP.
