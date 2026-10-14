//! User management.

use sqlx::SqlitePool;

use crate::auth::{find_user_by_name, hash_password};
use crate::error::AppError;
use crate::models::User;

/// Create a user, failing cleanly on a duplicate username.
pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password: &str,
    is_admin: bool,
) -> Result<User, AppError> {
    validate_username(username)?;
    validate_password(password)?;

    if find_user_by_name(pool, username).await?.is_some() {
        return Err(AppError::conflict(format!(
            "user '{username}' already exists"
        )));
    }

    let hash = hash_password(password)
        .map_err(|e| AppError::Internal(e.context("failed to hash password")))?;

    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, is_admin) VALUES (?, ?, ?, ?)",
    )
    .bind(username)
    .bind(email)
    .bind(hash)
    .bind(is_admin as i64)
    .execute(pool)
    .await?;

    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(result.last_insert_rowid())
        .fetch_one(pool)
        .await?;

    Ok(user)
}

/// Change a password (self-service or admin reset).
pub async fn set_password(
    pool: &SqlitePool,
    user_id: i64,
    new_password: &str,
) -> Result<(), AppError> {
    validate_password(new_password)?;
    let hash = hash_password(new_password)
        .map_err(|e| AppError::Internal(e.context("failed to hash password")))?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bootstrap the first administrator. No-op when the username already exists.
pub async fn ensure_admin(
    pool: &SqlitePool,
    username: &str,
    email: &str,
    password: &str,
) -> anyhow::Result<bool> {
    if find_user_by_name(pool, username).await?.is_some() {
        return Ok(false);
    }
    create_user(pool, username, Some(email), password, true)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create bootstrap admin: {e}"))?;
    Ok(true)
}

pub async fn count_users(pool: &SqlitePool) -> Result<i64, AppError> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

fn validate_username(username: &str) -> Result<(), AppError> {
    let len = username.chars().count();
    if !(2..=64).contains(&len) {
        return Err(AppError::bad_request(
            "username must be between 2 and 64 characters",
        ));
    }
    let ok = username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(AppError::bad_request(
            "username may only contain letters, digits, '-', '_' and '.'",
        ));
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), AppError> {
    if password.chars().count() < 8 {
        return Err(AppError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    Ok(())
}
