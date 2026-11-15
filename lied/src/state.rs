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

use crate::auth::jwt::Keyring;
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
    /// HS256 bearer-token signing/verification keyring, seeded from
    /// `config.jwt_signing_key` (see [`crate::auth::jwt::Keyring`]).
    pub jwt_keyring: Arc<Keyring>,
}

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("failed to connect to postgres: {0}")]
    Database(#[from] sqlx::Error),
    #[error("failed to apply database migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("failed to install metrics recorder: {0}")]
    Metrics(#[from] metrics_exporter_prometheus::BuildError),
}

/// The schema migrations under `/migrations`, embedded in the binary at
/// compile time (issue #87). [`AppState::connect`] applies them, so the
/// server and the `create-admin` CLI both find the schema they were built
/// against.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

impl AppState {
    /// Connect to Postgres, bring its schema up to date, and construct the S3
    /// client from config.
    ///
    /// Pending migrations are applied before anything else touches the
    /// database. sqlx holds a Postgres advisory lock while it migrates, so
    /// several instances starting at once apply each migration exactly once,
    /// and an already-current schema is a no-op. A database carrying a
    /// migration this binary does not know (an older binary against a newer
    /// schema), or one whose applied migration was edited since, fails
    /// startup rather than serving against a schema it was not built for.
    ///
    /// Creates the configured bucket if it is missing (issue #91), but an
    /// unreachable MinIO at boot does not crash the process (it may come up
    /// slightly later in `docker compose up`): that is logged, and `/readyz`
    /// is responsible for live health checks.
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

        MIGRATOR.run(&db).await?;
        tracing::info!("database schema is current");

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

        // A fresh deployment has no bucket (issue #91). Object storage may
        // still be starting, so a failure here is logged, not fatal; /readyz
        // keeps reporting it until it is reachable.
        let bucket_ready = match tokio::time::timeout(
            Duration::from_secs(10),
            crate::storage::ensure_bucket(&s3, &config.s3_bucket),
        )
        .await
        {
            Ok(Ok(crate::storage::BucketStatus::Created)) => {
                tracing::info!(bucket = %config.s3_bucket, "created object-storage bucket");
                true
            }
            Ok(Ok(crate::storage::BucketStatus::Existed)) => true,
            Ok(Err(error)) => {
                tracing::warn!(bucket = %config.s3_bucket, %error, "could not ensure the object-storage bucket exists");
                false
            }
            Err(_) => {
                tracing::warn!(bucket = %config.s3_bucket, "object storage did not answer at startup; the bucket was not checked");
                false
            }
        };
        // Versioning and the retention of old versions (#59), with the same
        // posture: a warning, not a failed start.
        if bucket_ready {
            match tokio::time::timeout(
                Duration::from_secs(10),
                crate::storage::ensure_versioning(
                    &s3,
                    &config.s3_bucket,
                    config.s3_version_retention_days,
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(bucket = %config.s3_bucket, %error, "could not set bucket versioning and version retention");
                }
                Err(_) => {
                    tracing::warn!(bucket = %config.s3_bucket, "object storage did not answer; bucket versioning was not set");
                }
            }
        }

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

        let jwt_keyring = Arc::new(Keyring::from_single_key(
            config.jwt_signing_key.expose(),
            config.jwt_lifetime_days,
        ));

        Ok(Self {
            config: Arc::new(config),
            db,
            s3,
            metrics,
            jwt_keyring,
        })
    }
}
