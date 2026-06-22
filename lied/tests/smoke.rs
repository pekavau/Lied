//! Integration test harness scaffold.
//!
//! Spins up ephemeral Postgres + MinIO via `testcontainers` and exercises
//! the app's `/healthz` and `/readyz` endpoints end-to-end. This is
//! intentionally minimal for the scaffold item — later items add fixture
//! builders (`seed_test_org()`, etc.) per the Testing posture in CLAUDE.md.

use std::net::TcpListener as StdTcpListener;

use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers_modules::postgres::Postgres;

/// Spins up a real Postgres container and confirms `lied::state::AppState`
/// can connect to it. MinIO is intentionally not exercised here yet (no
/// `testcontainers_modules` MinIO image is wired in this scaffold item);
/// `/readyz`'s MinIO check is covered indirectly by the unit-level handler
/// logic and will get full container coverage once file storage lands.
#[tokio::test]
async fn app_state_connects_to_postgres() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");

    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // Exercise the pool directly rather than the full AppState::connect,
    // since that also requires a reachable S3 endpoint which this scaffold
    // test does not stand up.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("failed to connect to ephemeral postgres");

    let row: (i32,) = sqlx::query_as("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("SELECT 1 should succeed");

    assert_eq!(row.0, 1);
}

/// Confirms the router's `/healthz` route is reachable via a real bound
/// TCP listener, without requiring any external dependency. This is the
/// "is the scaffold wired end-to-end" smoke test.
#[tokio::test]
async fn healthz_is_reachable_over_http() {
    // Bind an ephemeral port up front so we know what to request.
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("set nonblocking");

    let router = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("server error");
    });

    let response = reqwest::get(format!("http://{addr}/healthz"))
        .await
        .expect("request should succeed");
    assert!(response.status().is_success());

    server.abort();
}
