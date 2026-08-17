#![deny(unsafe_code)]

use std::path::PathBuf;

use anyhow::Result;

use alexandria_core::config::Settings;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::build_services;

use alexandria_http::app;
use alexandria_http::middleware::logging::init_tracing;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var("ALEXANDRIA_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let settings = Settings::load_or_default(&config_path);

    init_tracing(&settings.logging.level);

    // UC-36: external mode cannot verify a token without the Heimdall signing
    // secret and the scope it accepts. Refuse to start rather than answer 401
    // to every request for the life of the process.
    settings.auth.validate()?;

    let bind_addr = settings.http.socket_addr();
    let auth_mode = settings.auth.mode;

    let pool = migrate_database(&settings.database.path).await?;
    tracing::info!("database migrations applied");

    let services = std::sync::Arc::new(build_services(&settings, pool).await);

    tracing::info!(%bind_addr, auth_mode = auth_mode.as_str(), "starting alexandria-http");

    let router = app(settings, services);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
