//! API token lifecycle.

use sqlx::SqlitePool;

use crate::auth::{generate_token, hash_token, token_prefix};
use crate::error::AppError;
use crate::models::{Token, TokenView};

/// Create a token for a user. Returns the row plus the **plaintext** token, which
/// is the only moment the caller can ever see it.
pub async fn create_token(
    pool: &SqlitePool,
    user_id: i64,
    name: &str,
    source: &str,
    ttl_days: Option<i64>,
) -> Result<(Token, String), AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("token name is required"));
    }

    let plaintext = generate_token();
    let hash = hash_token(&plaintext);
    let prefix = token_prefix(&plaintext);
    let expires_at = ttl_days.map(crate::util::rfc3339_in_days);

    let result = sqlx::query(
        "INSERT INTO tokens (user_id, name, token_hash, token_prefix, source, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(name)
    .bind(&hash)
    .bind(&prefix)
    .bind(source)
    .bind(expires_at)
    .execute(pool)
    .await?;

    let token = sqlx::query_as::<_, Token>("SELECT * FROM tokens WHERE id = ?")
        .bind(result.last_insert_rowid())
        .fetch_one(pool)
        .await?;

    Ok((token, plaintext))
}

pub async fn list_tokens(pool: &SqlitePool, user_id: i64) -> Result<Vec<Token>, AppError> {
    let tokens = sqlx::query_as::<_, Token>(
        "SELECT * FROM tokens WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(tokens)
}

pub fn to_views(tokens: &[Token]) -> Vec<TokenView> {
    tokens.iter().map(TokenView::from).collect()
}

pub async fn revoke_token(pool: &SqlitePool, user_id: i64, token_id: i64) -> Result<(), AppError> {
    let result = sqlx::query("DELETE FROM tokens WHERE id = ? AND user_id = ?")
        .bind(token_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::not_found("token not found"));
    }
    Ok(())
}

pub async fn revoke_all(pool: &SqlitePool, user_id: i64) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM tokens WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
