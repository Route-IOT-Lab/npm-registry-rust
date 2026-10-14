//! Database pool creation and migration application.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

use crate::config::Config;

/// Build the connection pool, turning on the pragmas we rely on, then run any
/// pending migrations. Safe to call repeatedly — migrations are tracked by sqlx.
pub async fn init_pool(config: &Config) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&config.database_url)
        .with_context(|| format!("invalid DATABASE_URL '{}'", config.database_url))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10))
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

    // Kept modest on purpose. SQLite allows a single writer, so a large pool buys
    // nothing but contention (and one background thread plus buffers per
    // connection, on a host where memory is the scarce resource).
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options)
        .await
        .context("failed to connect to the database")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run database migrations")?;

    Ok(pool)
}
