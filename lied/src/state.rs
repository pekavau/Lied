//! Shared application state: config, DB pool, and S3 (MinIO) client.
//!
//! Constructed once at startup and cloned (cheaply — internals are `Arc`'d
//! by their respective crates) into every request handler via axum's
//! `State` extractor.

use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client as S3Client;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: PgPool,
    pub s3: S3Client,
    /// Render handle for the globally-installed Prometheus recorder.
    /// `Some` only when `LIED_METRICS_ENABLED` is set: installing the recorder
    /// is what makes `metrics::counter!`/`histogram!` calls actually record,
    /// and `/metrics` renders this handle. `None` when metrics are disabled,
    /// in which case no recorder is installed (the facade discards) and the
    /// endpoint returns 404.
    pub metrics: Option<PrometheusHandle>,
}

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("failed to connect to postgres: {0}")]
    Database(#[from] sqlx::Error),
    #[error("failed to install metrics recorder: {0}")]
    Metrics(#[from] metrics_exporter_prometheus::BuildError),
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

        // Install the global Prometheus recorder once, here at startup, only
        // when metrics are enabled. `install_recorder` both registers the
        // recorder globally (so metric macros anywhere record into it) and
        // returns the handle `/metrics` renders on demand. When disabled we
        // install nothing, so the `metrics` facade is a no-op with zero
        // overhead and the endpoint stays dark.
        let metrics = if config.metrics_enabled {
            Some(PrometheusBuilder::new().install_recorder()?)
        } else {
            None
        };

        Ok(Self {
            config: Arc::new(config),
            db,
            s3,
            metrics,
        })
    }
}
