//! Authentication & authorisation.
//!
//! Three credential shapes are accepted, in priority order:
//!
//! 1. `Authorization: Bearer <npmp_...>` — the opaque, revocable token npm stores
//!    in `.npmrc` after `npm login`.
//! 2. `Authorization: Basic <base64(user:pass)>` — legacy tooling / curl.
//! 3. `Cookie: <SESSION_COOKIE>=<jwt>` — the web dashboard session.
//!
//! Handlers never parse headers themselves; they declare the extractor they need.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::error::AppError;
use crate::models::{Token, User};
use crate::state::AppState;
use crate::util::{is_expired, now_rfc3339};

/// Browser session cookie name.
pub const SESSION_COOKIE: &str = "npm_registry_session";
/// Every API token we mint starts with this, so leaked ones are greppable.
pub const TOKEN_PREFIX: &str = "npmp_";

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))
}

pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    match PasswordHash::new(stored_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------

/// Mint a fresh opaque token. Only the caller sees the plaintext, and only once.
pub fn generate_token() -> String {
    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    format!("{TOKEN_PREFIX}{}", hex::encode(raw))
}

/// Tokens are stored hashed — a database leak does not yield usable credentials.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// A display-safe prefix, e.g. `npmp_a1b2c3d4…`.
pub fn token_prefix(token: &str) -> String {
    token.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// Web session JWTs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: i64,
    pub username: String,
    pub adm: bool,
    pub iat: i64,
    pub exp: i64,
}

pub fn create_session_token(state: &AppState, user: &User) -> Result<String, AppError> {
    let now = chrono::Utc::now();
    let claims = SessionClaims {
        sub: user.id,
        username: user.username.clone(),
        adm: user.is_admin(),
        iat: now.timestamp(),
        exp: (now + chrono::Duration::minutes(state.config.session_ttl_minutes)).timestamp(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    )
    .map_err(AppError::from)
}

pub fn verify_session_token(state: &AppState, token: &str) -> Result<SessionClaims, AppError> {
    let data = decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(state.config.jwt_secret.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}

// ---------------------------------------------------------------------------
// User lookup
// ---------------------------------------------------------------------------

pub async fn find_user_by_id(pool: &SqlitePool, id: i64) -> Result<Option<User>, AppError> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

pub async fn find_user_by_name(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<User>, AppError> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Resolve the caller from whichever credential shape they presented.
async fn resolve_identity(state: &AppState, parts: &Parts) -> Result<Option<User>, AppError> {
    // 1 + 2 — Authorization header.
    if let Some(header) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(raw) = header.to_str() {
            if let Some(token) = raw.strip_prefix("Bearer ").map(str::trim) {
                if let Some(user) = user_from_token(state, token).await? {
                    return Ok(Some(user));
                }
                return Ok(None);
            }
            if let Some(encoded) = raw.strip_prefix("Basic ").map(str::trim) {
                return user_from_basic(state, encoded).await;
            }
        }
    }

    // 3 — Session cookie.
    let jar = CookieJar::from_headers(&parts.headers);
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        if let Ok(claims) = verify_session_token(state, cookie.value()) {
            if let Some(user) = find_user_by_id(&state.pool, claims.sub).await? {
                return Ok(Some(user));
            }
        }
    }

    Ok(None)
}

async fn user_from_token(state: &AppState, token: &str) -> Result<Option<User>, AppError> {
    let hash = hash_token(token);
    let found = sqlx::query_as::<_, Token>("SELECT * FROM tokens WHERE token_hash = ?")
        .bind(&hash)
        .fetch_optional(&state.pool)
        .await?;

    let Some(tok) = found else {
        return Ok(None);
    };
    if let Some(expires_at) = &tok.expires_at {
        if is_expired(expires_at) {
            return Ok(None);
        }
    }

    let user = find_user_by_id(&state.pool, tok.user_id).await?;
    if user.is_none() {
        return Ok(None);
    }

    // Fire-and-forget touch; a failure here must not fail the request.
    let pool = state.pool.clone();
    let id = tok.id;
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?")
            .bind(now_rfc3339())
            .bind(id)
            .execute(&pool)
            .await;
    });

    Ok(user)
}

async fn user_from_basic(state: &AppState, encoded: &str) -> Result<Option<User>, AppError> {
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Ok(None);
    };
    let Ok(pair) = String::from_utf8(decoded) else {
        return Ok(None);
    };
    let Some((username, password)) = pair.split_once(':') else {
        return Ok(None);
    };

    let Some(user) = find_user_by_name(&state.pool, username).await? else {
        return Ok(None);
    };
    if !user.is_active() {
        return Ok(None);
    }
    if verify_password(password, &user.password_hash) {
        Ok(Some(user))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// Requires an authenticated, active user. Rejects with 401 otherwise.
#[derive(Debug, Clone)]
pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match resolve_identity(state, parts).await? {
            Some(user) if user.is_active() => Ok(AuthUser(user)),
            Some(_) => Err(AppError::forbidden("account is disabled")),
            None => Err(AppError::unauthorized(
                "authentication required: log in with `npm login` or the web dashboard",
            )),
        }
    }
}

/// Requires an authenticated administrator.
#[derive(Debug, Clone)]
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthUser(user) = AuthUser::from_request_parts(parts, state).await?;
        if user.is_admin() {
            Ok(AdminUser(user))
        } else {
            Err(AppError::forbidden("administrator privileges required"))
        }
    }
}

/// Never rejects — used where an action is allowed for anonymous callers.
#[derive(Debug, Clone)]
pub struct MaybeAuthUser(pub Option<User>);

impl FromRequestParts<AppState> for MaybeAuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(MaybeAuthUser(resolve_identity(state, parts).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let hash = hash_password("s3cret-pw").unwrap();
        assert!(verify_password("s3cret-pw", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    #[test]
    fn token_hash_is_deterministic() {
        let t = generate_token();
        assert!(t.starts_with(TOKEN_PREFIX));
        assert_eq!(hash_token(&t), hash_token(&t));
        assert_ne!(hash_token(&t), t);
    }
}
