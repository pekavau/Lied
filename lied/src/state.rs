//! Shared application state: config, DB pool, and S3 (MinIO) client.
//!
//! Constructed once at startup and cloned (cheaply — internals are `Arc`'d
//! by their respective crates) into every request handler via axum's
//! `State` extractor.

use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client as S3Client;
use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: PgPool,
    pub s3: S3Client,
}

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("failed to connect to postgres: {0}")]
    Database(#[from] sqlx::Error),
}

impl AppState {
    /// Connect to Postgres and construct the S3 client from config.
    ///
    /// Does not verify MinIO connectivity at startup — `/readyz` is
    /// responsible for live health checks; an unreachable MinIO at boot
    /// should not crash the process (it may come up slightly later in
    /// `docker compose up`).
    pub async fn connect(config: AppConfig) -> Result<Self, StateError> {
        let db = PgPoolOptions::new()
            .max_connections(10)
            // Bound how long a handler will wait for a pool connection. The
            // default is 30s, which turns `/readyz` (and any query) into a
            // 30s hang when Postgres is down instead of a prompt failure.
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(
                config
                    .database_url
                    .expose()
                    .parse::<sqlx::postgres::PgConnectOptions>()
                    .unwrap_or_else(|_| sqlx::postgres::PgConnectOptions::new())
                    .ssl_mode(PgSslMode::Prefer),
            )
            .await?;

        let s3_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .endpoint_url(&config.s3_endpoint)
            .region(aws_sdk_s3::config::Region::new(config.s3_region.clone()))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                config.s3_access_key_id.expose(),
                config.s3_secret_access_key.expose(),
                None,
                None,
                "lied-static",
            ))
            .load()
            .await;

        let s3 = S3Client::from_conf(
            aws_sdk_s3::config::Builder::from(&s3_config)
                // MinIO requires path-style addressing (bucket.endpoint.com
                // virtual-hosted addressing doesn't work against a local MinIO).
                .force_path_style(true)
                .build(),
        );

        Ok(Self {
            config: Arc::new(config),
            db,
            s3,
        })
    }
}
