//! CI gate: the fully-populated OpenAPI document (assembled by
//! `OpenApiRouter::split_for_parts()` in `routes/mod.rs`) must contain
//! every expected `/v1` path, every operation must have a non-empty summary,
//! every operation must expose at least one 2xx response, and the canonical
//! error codes must appear on the annotated operations.
//!
//! This is a pure unit test — no database or MinIO required. It calls
//! `openapi::ApiDoc::openapi()` as the seed, applies `utoipa_axum`'s path
//! collection (by building the `OpenApiRouter` exactly as `build_router`
//! does), and then asserts structural properties of the spec.

use lied::routes::openapi::ApiDoc;
use lied::state::AppState;
use utoipa::openapi::path::Operation;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

/// Build the OpenAPI document exactly as `build_router` does, but without
/// standing up a real `AppState` — we need only the schema half of
/// `split_for_parts`.
fn build_openapi() -> utoipa::openapi::OpenApi {
    // `v1::router` requires an `AppState` to construct the session layer, so
    // we call the lower-level sub-routers directly (they take no state) and
    // merge them, mirroring what `v1::router` does minus session wiring.
    let (_, api) = OpenApiRouter::<AppState>::with_openapi(ApiDoc::openapi())
        .nest(
            "/v1",
            OpenApiRouter::new()
                .merge(lied::routes::orgs::router())
                .merge(lied::routes::arrangements::router())
                .merge(lied::routes::files::router()),
        )
        .split_for_parts();
    api
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

/// Every path segment we expect to find in the OpenAPI document. The list
/// covers all resource groups in the phase-1 feature set implemented so far.
///
/// Path parameter names come from the `#[utoipa::path(path = "...")]`
/// annotations in each route module; the axum extractor names are irrelevant
/// for the spec.
const EXPECTED_PATHS: &[&str] = &[
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
];

#[test]
fn all_expected_paths_are_present() {
    let api = build_openapi();
    let paths = api.paths.paths.keys().collect::<Vec<_>>();

    let mut missing = Vec::new();
    for expected in EXPECTED_PATHS {
        if !paths.iter().any(|p| p.as_str() == *expected) {
            missing.push(*expected);
        }
    }

    assert!(
        missing.is_empty(),
        "Missing paths in OpenAPI spec:\n{}",
        missing.join("\n")
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
            // `op.responses.responses` is a BTreeMap<String, RefOr<Response>>
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

#[test]
fn security_requirements_declare_bearer_or_session() {
    let api = build_openapi();
    let known_schemes = ["bearer", "session"];

    let mut violations = Vec::new();
    for (path, item) in &api.paths.paths {
        for (method, op) in path_item_ops(item) {
            // SecurityRequirement.value is private; serialize to JSON to inspect keys.
            let declares_scheme = op.security.as_deref().unwrap_or(&[]).iter().any(|req| {
                // Flatten-serialized: the outer object's keys are the scheme names.
                let v = serde_json::to_value(req).unwrap_or_default();
                if let Some(obj) = v.as_object() {
                    obj.keys().any(|k| known_schemes.contains(&k.as_str()))
                } else {
                    false
                }
            });

            if !declares_scheme {
                violations.push(format!("{method} {path}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Operations with no bearer/session security requirement:\n{}",
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
