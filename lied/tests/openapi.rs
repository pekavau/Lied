//! CI gate: the fully-populated OpenAPI document (assembled by
//! `OpenApiRouter::split_for_parts()` in `routes/mod.rs`) must obey the API
//! guidelines (`docs/api-guidelines.md`). This test enforces the mechanical,
//! machine-checkable subset:
//!
//! - **Completeness (§2).** The set of documented `/v1` paths equals the
//!   canonical `EXPECTED_PATHS` list exactly — a new endpoint or a typo in a
//!   `#[utoipa::path(path = …)]` annotation fails until the list is updated
//!   (a deliberate review checkpoint). No `/v1` route may bypass the OpenAPI
//!   registration via a bare `.route(...)`.
//! - **Documentation (§2).** Every operation has a non-empty summary and at
//!   least one `2xx` response.
//! - **Auth (§3).** Every operation declares a `bearer`-or-`session` security
//!   requirement, or explicitly declares none (the unauthenticated endpoints).
//! - **Error model (§4).** Every *authenticated* operation carries the
//!   `CommonErrors` triple (401/429/500); every `PATCH` carries `412`; every
//!   `4xx`/`5xx` response body is the shared RFC 7807 `ProblemDetails` schema.
//! - **Casing (§5).** Every component-schema property name is `camelCase`
//!   (no snake_case leaking to the wire).
//!
//! This is a pure unit test — no database or MinIO required. It builds the
//! `/v1` tree from `v1::api_router` (the same source `build_router` uses in
//! production, minus the session layer, which does not affect the spec) so the
//! documented spec is exactly the served spec.

use lied::routes::openapi::ApiDoc;
use lied::state::AppState;
use serde_json::Value;
use utoipa::openapi::path::Operation;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

/// Build the OpenAPI document exactly as `build_router` does, but without
/// standing up a real `AppState` — we need only the schema half of
/// `split_for_parts`. `api_router` registers every `/v1` route with no
/// state-dependent middleware, so the login-rate-limit argument is irrelevant
/// to the emitted paths.
fn build_openapi() -> utoipa::openapi::OpenApi {
    let (_, api) = OpenApiRouter::<AppState>::with_openapi(ApiDoc::openapi())
        .nest("/v1", lied::routes::v1::api_router(10))
        .split_for_parts();
    api
}

/// The same document as JSON, for tests that walk the spec structurally
/// (properties, response bodies) without wrestling utoipa's internal types.
fn build_openapi_json() -> Value {
    serde_json::to_value(build_openapi()).expect("OpenAPI document serializes to JSON")
}

/// Collect all (method_label, &Operation) pairs from a PathItem.
fn path_item_ops(item: &utoipa::openapi::path::PathItem) -> Vec<(&'static str, &Operation)> {
    let mut ops = Vec::new();
    if let Some(op) = &item.get {
        ops.push(("GET", op));
    }
    if let Some(op) = &item.put {
        ops.push(("PUT", op));
    }
    if let Some(op) = &item.post {
        ops.push(("POST", op));
    }
    if let Some(op) = &item.delete {
        ops.push(("DELETE", op));
    }
    if let Some(op) = &item.patch {
        ops.push(("PATCH", op));
    }
    if let Some(op) = &item.head {
        ops.push(("HEAD", op));
    }
    if let Some(op) = &item.options {
        ops.push(("OPTIONS", op));
    }
    if let Some(op) = &item.trace {
        ops.push(("TRACE", op));
    }
    ops
}

/// Every path we expect to find in the OpenAPI document — the canonical `/v1`
/// surface. The equality test below asserts the spec matches this set *exactly*
/// (both directions), so this list is the reviewed contract: adding, removing,
/// or renaming an endpoint forces a conscious edit here.
///
/// Path parameter names come from the `#[utoipa::path(path = "...")]`
/// annotations in each route module; the axum extractor names are irrelevant
/// for the spec.
const EXPECTED_PATHS: &[&str] = &[
    // Meta / auth / instruments (v1.rs top level)
    "/v1",
    "/v1/instruments",
    "/v1/tokens",
    "/v1/app-passwords",
    "/v1/app-passwords/{id}",
    // Orgs
    "/v1/orgs",
    "/v1/orgs/{id}",
    // Users (admin — system-scoped)
    "/v1/users",
    "/v1/users/{id}",
    // Members
    "/v1/orgs/{orgId}/members",
    "/v1/orgs/{orgId}/members/{id}",
    // Works (instance-wide)
    "/v1/works",
    "/v1/works/{id}",
    // Arrangements
    "/v1/orgs/{orgId}/arrangements",
    "/v1/orgs/{orgId}/arrangements/{id}",
    "/v1/orgs/{orgId}/arrangements/{id}/undelete",
    // Voices
    "/v1/orgs/{orgId}/arrangements/{id}/voices",
    "/v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}",
    "/v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}/undelete",
    // Tags (org-level)
    "/v1/orgs/{orgId}/tags",
    "/v1/orgs/{orgId}/tags/{id}",
    "/v1/orgs/{orgId}/tags/{id}/undelete",
    // Arrangement-tag join
    "/v1/orgs/{orgId}/arrangements/{id}/tags",
    "/v1/orgs/{orgId}/arrangements/{arrangementId}/tags/{tagId}",
    // Full-score files
    "/v1/orgs/{orgId}/arrangements/{arrId}/files",
    "/v1/orgs/{orgId}/arrangements/{arrId}/files/{fileId}",
    // Voice files
    "/v1/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files",
    "/v1/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files/{fileId}",
    // Collections
    "/v1/orgs/{orgId}/collections",
    "/v1/orgs/{orgId}/collections/{id}",
    "/v1/orgs/{orgId}/collections/{id}/undelete",
    "/v1/orgs/{orgId}/collections/{id}/reorder",
    // Collection items
    "/v1/orgs/{orgId}/collections/{id}/items",
    "/v1/orgs/{orgId}/collections/{collectionId}/coverage",
    "/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}",
    "/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}/undelete",
    // Part assignments
    "/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments",
    "/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments/{assignmentId}",
];

/// §2 completeness: the documented path set equals `EXPECTED_PATHS` exactly.
/// Reports both missing (expected but absent) and unexpected (present but not
/// listed) so a drift in either direction fails loudly.
#[test]
fn path_set_matches_expected_exactly() {
    let api = build_openapi();
    let actual: std::collections::BTreeSet<&str> =
        api.paths.paths.keys().map(String::as_str).collect();
    let expected: std::collections::BTreeSet<&str> = EXPECTED_PATHS.iter().copied().collect();

    let missing: Vec<&str> = expected.difference(&actual).copied().collect();
    let unexpected: Vec<&str> = actual.difference(&expected).copied().collect();

    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "OpenAPI path set does not match EXPECTED_PATHS.\n\
         Missing (in EXPECTED_PATHS, absent from spec):\n  {}\n\
         Unexpected (in spec, not in EXPECTED_PATHS):\n  {}",
        if missing.is_empty() {
            "(none)".to_string()
        } else {
            missing.join("\n  ")
        },
        if unexpected.is_empty() {
            "(none)".to_string()
        } else {
            unexpected.join("\n  ")
        },
    );
}

/// §2: no `/v1` route may be wired with a bare `.route(...)`, which registers
/// the handler with axum but *not* with the OpenAPI document — the one hole
/// `split_for_parts` cannot reveal (axum's `Router` is not introspectable).
/// Every endpoint must go through the `routes!` macro. This guards the escape
/// hatch the guidelines reserve but say is unused in `/v1`.
#[test]
fn no_v1_route_bypasses_openapi_registration() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let routes_dir = format!("{manifest}/src/routes");

    // The non-`/v1` route trees legitimately use bare `.route(` — they are
    // plain `axum::Router`s, not `OpenApiRouter`s: the infra/admin/webdav trees
    // (`admin` is a subdirectory, skipped by the `.rs` filter), plus the module
    // root and the OpenAPI base doc. *Every other* `.rs` file under
    // `src/routes/` composes the `/v1` tree, so a newly added `/v1` module is
    // covered by this lint automatically without editing the test.
    let non_v1 = ["mod.rs", "openapi.rs", "infra.rs", "webdav.rs"];

    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&routes_dir).expect("routes dir readable") {
        let entry = entry.expect("dir entry");
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.ends_with(".rs") || non_v1.contains(&name.as_ref()) {
            continue;
        }
        let src = std::fs::read_to_string(entry.path()).expect("route module readable");
        for (n, line) in src.lines().enumerate() {
            // Ignore comment lines so doc-comments mentioning `.route(` don't
            // trip the lint. `.routes(`, `.route_layer(`, `.route_service(`
            // are all fine — only the bare `.route(` bypass is forbidden.
            let code = line.split("//").next().unwrap_or("");
            if code.contains(".route(") {
                offenders.push(format!("{name}:{}: {}", n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`/v1` route(s) bypass OpenAPI registration via bare `.route(` \
         (use the `routes!` macro instead):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_operation_has_a_non_empty_summary() {
    let api = build_openapi();

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        for (method, op) in path_item_ops(item) {
            if op.summary.as_deref().unwrap_or("").trim().is_empty() {
                violations.push(format!("{method} {path}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Operations with empty summary:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_operation_has_at_least_one_2xx_response() {
    let api = build_openapi();

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        for (method, op) in path_item_ops(item) {
            let has_2xx = op
                .responses
                .responses
                .keys()
                .any(|code| code.starts_with('2'));
            if !has_2xx {
                violations.push(format!("{method} {path}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Operations without a 2xx response:\n{}",
        violations.join("\n")
    );
}

/// Does an operation's `security` require one of our known schemes? An
/// authenticated operation lists `bearer`/`session`; an unauthenticated one
/// declares `security()` (an empty requirement list) so the UI shows no lock.
fn operation_is_authenticated(op: &Operation) -> bool {
    let known_schemes = ["bearer", "session"];
    op.security.as_deref().unwrap_or(&[]).iter().any(|req| {
        // SecurityRequirement.value is private; serialize to inspect keys.
        let v = serde_json::to_value(req).unwrap_or_default();
        v.as_object()
            .map(|obj| obj.keys().any(|k| known_schemes.contains(&k.as_str())))
            .unwrap_or(false)
    })
}

#[test]
fn security_requirements_declare_bearer_or_session_or_none() {
    let api = build_openapi();

    // Endpoints that mint or precede auth are intentionally public. Everything
    // else must require bearer/session.
    let public: std::collections::BTreeSet<(&str, &str)> = [("GET", "/v1"), ("POST", "/v1/tokens")]
        .into_iter()
        .collect();

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        for (method, op) in path_item_ops(item) {
            if public.contains(&(method, path.as_str())) {
                continue;
            }
            if !operation_is_authenticated(op) {
                violations.push(format!("{method} {path}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Authenticated operations missing a bearer/session security requirement:\n{}",
        violations.join("\n")
    );
}

/// §4: every authenticated operation can emit the `CommonErrors` triple, so its
/// documented responses must include 401, 429, and 500. Public endpoints are
/// exempt from 401 (they have no auth to fail).
#[test]
fn authenticated_operations_declare_common_errors() {
    let api = build_openapi();

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        for (method, op) in path_item_ops(item) {
            if !operation_is_authenticated(op) {
                continue;
            }
            let codes: std::collections::BTreeSet<&str> =
                op.responses.responses.keys().map(String::as_str).collect();
            let missing: Vec<&str> = ["401", "429", "500"]
                .into_iter()
                .filter(|c| !codes.contains(c))
                .collect();
            if !missing.is_empty() {
                violations.push(format!("{method} {path} — missing {}", missing.join(", ")));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Authenticated operations missing CommonErrors codes:\n{}",
        violations.join("\n")
    );
}

/// §4.3: every `PATCH` is an `If-Match`-guarded metadata update, so it must
/// document `412 Precondition Failed`. (DELETE/PUT 412 coverage is not asserted
/// here because detach/undelete DELETEs are deliberately un-guarded, which
/// needs per-endpoint classification the guidelines table carries but this
/// structural test does not.)
#[test]
fn patch_operations_declare_precondition_412() {
    let api = build_openapi();

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        if let Some(op) = &item.patch {
            let has_412 = op.responses.responses.keys().any(|c| c == "412");
            if !has_412 {
                violations.push(format!("PATCH {path}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "PATCH operations missing a 412 Precondition Failed response:\n{}",
        violations.join("\n")
    );
}

/// §4: every `4xx`/`5xx` response body is the shared RFC 7807 `ProblemDetails`
/// schema (referenced via `$ref`), never a bespoke error shape.
#[test]
fn error_responses_use_problem_details_schema() {
    let json = build_openapi_json();
    let paths = json["paths"].as_object().expect("paths object");

    let mut violations = Vec::new();
    for (path, item) in paths {
        let methods = item.as_object().expect("path item object");
        for (method, op) in methods {
            // Skip non-operation keys on a PathItem (parameters, summary, …).
            let Some(responses) = op.get("responses").and_then(Value::as_object) else {
                continue;
            };
            for (status, resp) in responses {
                if !(status.starts_with('4') || status.starts_with('5')) {
                    continue;
                }
                // Find any content schema and confirm it $refs ProblemDetails.
                let refs_problem_details = resp
                    .get("content")
                    .and_then(Value::as_object)
                    .map(|content| {
                        content.values().any(|media| {
                            media["schema"]["$ref"]
                                .as_str()
                                .map(|r| r.ends_with("/ProblemDetails"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !refs_problem_details {
                    violations.push(format!("{} {path} → {status}", method.to_uppercase()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Error responses whose body is not the ProblemDetails schema:\n{}",
        violations.join("\n")
    );
}

/// §5: wire JSON is `camelCase`. Every property name in every component schema
/// must be free of underscores (the snake_case tell). Catches a DTO field that
/// forgot `#[serde(rename_all = "camelCase")]` / `ToSchema` alignment.
#[test]
fn schema_properties_are_camel_case() {
    let json = build_openapi_json();
    let schemas = json["components"]["schemas"]
        .as_object()
        .expect("components.schemas object");

    let mut violations = Vec::new();
    for (schema_name, schema) in schemas {
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for prop in props.keys() {
            if prop.contains('_') {
                violations.push(format!("{schema_name}.{prop}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Schema properties that are not camelCase (contain '_'):\n{}",
        violations.join("\n")
    );
}

#[test]
fn openapi_document_has_expected_resource_tags() {
    let api = build_openapi();
    let tags: Vec<&str> = api
        .tags
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|t| t.name.as_str())
        .collect();

    for expected_tag in [
        "works",
        "arrangements",
        "voices",
        "tags",
        "files",
        "organizations",
        "members",
    ] {
        assert!(
            tags.contains(&expected_tag),
            "Missing expected tag '{expected_tag}' in OpenAPI document tags"
        );
    }
}
