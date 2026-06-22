//! Tracing setup: JSON output in production, pretty output in development
//! (toggled by `AppConfig::tracing_pretty`, env var `LIED_TRACING_PRETTY`).
//!
//! Per-request request-ID spans are attached by `tower_http::trace::TraceLayer`
//! combined with a request-id-generating middleware in `routes`.

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::AppConfig;

/// Initialize the global tracing subscriber. Must be called once at startup,
/// before any other tracing calls.
pub fn init(config: &AppConfig) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(env_filter);

    if config.tracing_pretty {
        registry
            .with(tracing_subscriber::fmt::layer().pretty())
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().json().flatten_event(true))
            .init();
    }
}
