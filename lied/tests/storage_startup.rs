//! Issue #91: nothing created the object-storage bucket, so a fresh
//! deployment could store no file and `/readyz` stayed 503. Startup
//! (`AppState::connect`) now creates it when missing and leaves an existing
//! one alone.

use lied::config::{AppConfig, Secret};
use lied::state::AppState;
use lied::storage::{ensure_bucket, BucketStatus};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
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
    // Same MinIO image and readiness line as tests/files.rs; its built-in
    // credentials are minioadmin/minioadmin. It starts with no buckets.
    let minio = GenericImage::new("cgr.dev/chainguard/minio", "latest")
        .with_exposed_port(9000.tcp())
        .with_wait_for(WaitFor::message_on_stderr("API:"))
        .with_cmd(["server", "/data"])
        .start()
        .await
        .expect("start minio");
    let minio_port = minio.get_host_port_ipv4(9000).await.expect("minio port");

    let config = || AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from(format!(
            "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
        )),
        s3_endpoint: format!("http://127.0.0.1:{minio_port}"),
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from("minioadmin".to_string()),
        s3_secret_access_key: Secret::from("minioadmin".to_string()),
        s3_region: "us-east-1".to_string(),
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
    };

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
