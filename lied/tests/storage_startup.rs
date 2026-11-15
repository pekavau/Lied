//! Issue #91: nothing created the object-storage bucket, so a fresh
//! deployment could store no file and `/readyz` stayed 503. Startup
//! (`AppState::connect`) now creates it when missing and leaves an existing
//! one alone.

mod common;

use lied::config::{AppConfig, Secret};
use lied::state::AppState;
use lied::storage::{ensure_bucket, BucketStatus};
use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers_modules::postgres::Postgres;

const BUCKET: &str = "lied-startup";

#[tokio::test]
async fn startup_creates_a_missing_bucket_and_keeps_an_existing_one() {
    let pg = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    // The pinned RustFS image (tests/common); it starts with no buckets.
    let (_store, endpoint) = common::start_object_store().await;
    let config = || config_for(pg_port, &endpoint, 30);

    let state = AppState::connect(config()).await.expect("startup");
    state
        .s3
        .head_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("startup created the bucket");

    // Put something in it; a restart must neither fail nor disturb it.
    lied::storage::upload_bytes(
        &state.s3,
        BUCKET,
        "probe.txt",
        "text/plain",
        1024,
        bytes::Bytes::from_static(b"kept"),
    )
    .await
    .expect("the bucket accepts writes");
    let restarted = AppState::connect(config()).await.expect("restart");
    assert_eq!(
        ensure_bucket(&restarted.s3, BUCKET).await.unwrap(),
        BucketStatus::Existed
    );
    restarted
        .s3
        .head_object()
        .bucket(BUCKET)
        .key("probe.txt")
        .send()
        .await
        .expect("the existing contents are untouched");
}

/// The startup tests' configuration, against the given Postgres and object
/// store, with `retention_days` as `LIED_S3_VERSION_RETENTION_DAYS`.
fn config_for(pg_port: u16, endpoint: &str, retention_days: u32) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from(format!(
            "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
        )),
        s3_endpoint: endpoint.to_string(),
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from(common::ACCESS_KEY.to_string()),
        s3_secret_access_key: Secret::from(common::SECRET_KEY.to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: retention_days,
        jwt_signing_key: Secret::from("test-signing-key-material-for-storage".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1024,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 50,
        max_arrangements_per_org: None,
        max_page_size: 200,
        default_page_size: 50,
        ratelimit_auth_per_min: 120,
        ratelimit_anon_per_min: 20,
        ratelimit_login_per_min: 10,
        metrics_enabled: false,
        docs_enabled: true,
        tracing_pretty: false,
        secure_cookies: false,
    }
}

/// #59: startup turns bucket versioning on and sets how long old versions
/// are kept (`LIED_S3_VERSION_RETENTION_DAYS`). Overwriting an object keeps
/// its previous content as a noncurrent version; `0` keeps them forever, so
/// no lifecycle rule remains; any other value replaces the rule.
#[tokio::test]
async fn startup_turns_versioning_on_and_sets_the_version_retention() {
    use aws_sdk_s3::types::BucketVersioningStatus;

    let pg = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    let (_store, endpoint) = common::start_object_store().await;

    let state = AppState::connect(config_for(pg_port, &endpoint, 30))
        .await
        .expect("startup");
    let versioning = state
        .s3
        .get_bucket_versioning()
        .bucket(BUCKET)
        .send()
        .await
        .expect("get versioning");
    assert_eq!(versioning.status(), Some(&BucketVersioningStatus::Enabled));
    let rule_days = |state: &AppState| {
        let s3 = state.s3.clone();
        async move {
            match s3
                .get_bucket_lifecycle_configuration()
                .bucket(BUCKET)
                .send()
                .await
            {
                Ok(lifecycle) => {
                    let rules = lifecycle.rules();
                    assert_eq!(rules.len(), 1, "{rules:?}");
                    let rule = &rules[0];
                    assert_eq!(rule.id(), Some(lied::storage::VERSION_RETENTION_RULE_ID));
                    assert_eq!(
                        rule.expiration()
                            .and_then(|e| e.expired_object_delete_marker()),
                        Some(true),
                        "orphaned delete markers are removed too"
                    );
                    rule.noncurrent_version_expiration()
                        .and_then(|e| e.noncurrent_days())
                }
                Err(_) => None,
            }
        }
    };
    assert_eq!(rule_days(&state).await, Some(30));

    // An overwrite keeps the previous content as a noncurrent version.
    for content in [&b"first"[..], &b"second"[..]] {
        lied::storage::upload_bytes(
            &state.s3,
            BUCKET,
            "part.pdf",
            "application/pdf",
            1024,
            bytes::Bytes::copy_from_slice(content),
        )
        .await
        .expect("upload");
    }
    let versions = state
        .s3
        .list_object_versions()
        .bucket(BUCKET)
        .prefix("part.pdf")
        .send()
        .await
        .expect("list versions");
    let versions = versions.versions();
    assert_eq!(versions.len(), 2, "{versions:?}");
    assert_eq!(
        versions
            .iter()
            .filter(|v| v.is_latest() == Some(true))
            .count(),
        1
    );
    let old = versions
        .iter()
        .find(|v| v.is_latest() != Some(true))
        .and_then(|v| v.version_id())
        .expect("a noncurrent version")
        .to_string();
    let old_body = state
        .s3
        .get_object()
        .bucket(BUCKET)
        .key("part.pdf")
        .version_id(old)
        .send()
        .await
        .expect("get the old version")
        .body
        .collect()
        .await
        .expect("read")
        .into_bytes();
    assert_eq!(&old_body[..], b"first");

    // 0 keeps every version: the rule is removed.
    let keep_all = AppState::connect(config_for(pg_port, &endpoint, 0))
        .await
        .expect("restart with 0");
    assert_eq!(rule_days(&keep_all).await, None);

    // Any other value sets it again.
    let week = AppState::connect(config_for(pg_port, &endpoint, 7))
        .await
        .expect("restart with 7");
    assert_eq!(rule_days(&week).await, Some(7));
}
