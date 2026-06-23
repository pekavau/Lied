#![forbid(unsafe_code)]

use std::net::SocketAddr;

use anyhow::Context;

mod cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(command) = cli::parse_args(std::env::args().skip(1)) {
        return cli::run(command).await;
    }

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

    // `with_connect_info` (rather than the plain `into_make_service`) is
    // required so the login rate-limiter's `PeerIpKeyExtractor` can read
    // the client's peer IP out of `ConnectInfo` (see
    // `lied::auth::ratelimit`).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("server error")?;

    Ok(())
}
