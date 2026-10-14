//! npm-registry-rust — a lightweight, self-hosted private npm registry.
//!
//! `main` is deliberately only wiring: configuration, observability, the
//! database, and the HTTP server. All behaviour lives in the modules below.

mod auth;
mod config;
mod db;
mod error;
mod models;
mod routes;
mod service;
mod state;
mod storage;
mod upstream;
mod util;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::{header, HeaderValue, Method};
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;
use crate::storage::Storage;

#[tokio::main]
async fn main() -> Result<()> {
    // `.env` is optional; real deployments inject variables directly.
    let _ = dotenvy::dotenv();

    // Configuration is validated before anything else so a bad deployment fails
    // immediately and loudly, with every problem listed at once.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            std::process::exit(2);
        }
    };

    init_tracing(&config);
    config::ensure_dirs(&config)?;

    let pool = db::init_pool(&config).await?;
    bootstrap_admin(&config, &pool).await?;

    let storage = Storage::new(config.storage_dir.clone());
    let upstream = upstream::UpstreamClient::build(&config)?;
    let state = AppState::new(Arc::new(config.clone()), pool, storage, upstream);

    let app = routes::build_router(state.clone())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().include_headers(false))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(build_cors(&config))
        .layer(RequestBodyLimitLayer::new(config.max_publish_body_bytes));

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr()?;

    match &state.upstream {
        Some(client) => tracing::info!(
            upstreams = %client.registries().join(" -> "),
            "upstream proxying enabled (relayed in order, streamed not cached)"
        ),
        None => tracing::info!("upstream proxying disabled — unknown packages resolve to 404"),
    }

    tracing::info!(
        address = %local,
        registry = %config.registry_base_url,
        storage = %config.storage_dir.display(),
        "npm-registry-rust is ready"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("shutdown complete");
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);
    if config.log_format == config::LogFormat::Json {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true),
            )
            .init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }
}

/// Create the bootstrap administrator on first boot, and shout if the instance
/// ends up with no way in at all.
async fn bootstrap_admin(config: &Config, pool: &sqlx::SqlitePool) -> Result<()> {
    let existing = service::users::count_users(pool)
        .await
        .map_err(|e| anyhow::anyhow!("failed to count users: {e}"))?;

    if existing == 0 {
        if config.admin_username.is_empty() || config.admin_password.is_empty() {
            tracing::warn!(
                "no users exist and ADMIN_USERNAME / ADMIN_PASSWORD are unset — \
                 set them and restart to create the first administrator"
            );
            return Ok(());
        }
        service::users::ensure_admin(
            pool,
            &config.admin_username,
            &config.admin_email,
            &config.admin_password,
        )
        .await?;
        tracing::info!(username = %config.admin_username, "bootstrap administrator created");
    } else if !config.admin_username.is_empty() {
        // Keep the requested admin present even if other users exist.
        if service::users::ensure_admin(
            pool,
            &config.admin_username,
            &config.admin_email,
            &config.admin_password,
        )
        .await?
        {
            tracing::info!(username = %config.admin_username, "administrator created");
        }
    }

    Ok(())
}

fn build_cors(config: &Config) -> CorsLayer {
    let origins = &config.cors_allowed_origins;

    if origins.is_empty() {
        // No cross-origin callers configured: emit no CORS headers at all.
        return CorsLayer::new();
    }

    if origins.iter().any(|o| o == "*") {
        tracing::warn!(
            "CORS_ALLOWED_ORIGINS contains '*' — credentials are disabled and this must not be \
             used in production. Configure explicit origins instead."
        );
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }

    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %o, "ignoring invalid CORS origin");
                None
            }
        })
        .collect();

    CorsLayer::new()
        .allow_origin(parsed)
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
}

/// Resolve on Ctrl-C or SIGTERM so in-flight requests can finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
