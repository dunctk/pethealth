mod agent;
mod auth;
mod config;
mod db;
mod domain;
mod mcp;
mod ocr;
mod web;

use anyhow::Context;
use config::Config;
use std::sync::Arc;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub db: sea_orm::DatabaseConnection,
    pub config: Arc<Config>,
    pub agent: Arc<agent::CaptureAgent>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pethealth=info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    let database = db::connect(&config.database_url).await?;
    db::migrate(&database).await?;
    db::bootstrap_owner(&database, &config.username, &config.password).await?;
    let agent = Arc::new(agent::CaptureAgent::new(&config));
    let state = AppState {
        db: database,
        config: config.clone(),
        agent,
    };

    let app = web::router(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());
    let address = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    info!(address, database = %config.database_url, production = config.production, "pethealth ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
