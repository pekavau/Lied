#![forbid(unsafe_code)]

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = lied::config::AppConfig::load().context("failed to load configuration")?;

    lied::telemetry::init(&config);

    tracing::info!(
        bind_addr = %config.bind_addr,
        "starting lied-server"
    );

    let state = lied::state::AppState::connect(config)
        .await
        .context("failed to initialize application state")?;

    let bind_addr = state.config.bind_addr;
    let app = lied::routes::build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind to {bind_addr}"))?;

    tracing::info!(%bind_addr, "listening");

    axum::serve(listener, app).await.context("server error")?;

    Ok(())
}
