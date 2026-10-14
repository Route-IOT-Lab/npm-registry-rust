//! JSON API backing the dashboard.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{
    self, create_session_token, verify_password, AdminUser, AuthUser, SESSION_COOKIE,
};
use crate::error::{AppError, AppResult};
use crate::models::{UserView, Version};
use crate::service::{packages as pkg_service, tokens as token_service, users as user_service};
use crate::state::AppState;
use crate::util::{now_rfc3339, rfc3339_in_minutes};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/config", get(config))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/register", post(register))
        .route("/api/me", get(me))
        .route("/api/me/password", post(change_password))
        .route("/api/me/tokens", get(list_tokens).post(create_token))
        .route("/api/me/tokens/{id}", delete(revoke_token))
        .route("/api/packages", get(list_packages))
        .route(
            "/api/packages/{*name}",
            get(package_detail).delete(delete_package),
        )
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", delete(delete_user))
        .route("/api/users/{id}/password", post(reset_password))
        .route("/api/cli/authorize", post(cli_authorize))
        .route("/api/stats", get(stats))
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "memory_kb": resident_memory_kb(),
    }))
}

/// Resident set size straight from procfs. Only meaningful on Linux, which is
/// where this actually runs; elsewhere it reports `null`.
///
/// Exposed because a registry that proxies upstreams can legitimately use
/// hundreds of megabytes under a burst of concurrent installs, and the number
/// belongs somewhere a human can see it without shelling into the box.
fn resident_memory_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
}

async fn config(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "registry_base_url": state.config.registry_base_url,
        "allow_registration": state.config.allow_registration,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn stats(State(state): State<AppState>, _admin: AdminUser) -> AppResult<Json<Value>> {
    let (users,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await?;
    let (packages,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM packages")
        .fetch_one(&state.pool)
        .await?;
    let (versions,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM versions")
        .fetch_one(&state.pool)
        .await?;
    let (downloads,): (i64,) = sqlx::query_as("SELECT COALESCE(SUM(downloads), 0) FROM packages")
        .fetch_one(&state.pool)
        .await?;

    Ok(Json(json!({
        "users": users,
        "packages": packages,
        "versions": versions,
        "downloads": downloads,
        "upstream_registries": state
            .upstream
            .as_ref()
            .map(|c| c.registries().to_vec())
            .unwrap_or_default(),
    })))
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> AppResult<Response> {
    let Some(user) = auth::find_user_by_name(&state.pool, body.username.trim()).await? else {
        return Err(AppError::unauthorized("用户名或密码错误"));
    };
    if !user.is_active() {
        return Err(AppError::forbidden("账号已被禁用"));
    }
    if !verify_password(&body.password, &user.password_hash) {
        return Err(AppError::unauthorized("用户名或密码错误"));
    }

    let token = create_session_token(&state, &user)?;
    let cookie = session_cookie(&state, &token);

    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "user": UserView::from(&user) })),
    )
        .into_response())
}

async fn logout() -> Response {
    (
        [(header::SET_COOKIE, clear_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct RegisterRequest {
    username: String,
    #[serde(default)]
    email: Option<String>,
    password: String,
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> AppResult<Response> {
    if !state.config.allow_registration {
        return Err(AppError::forbidden("本实例未开放注册"));
    }
    let user = user_service::create_user(
        &state.pool,
        body.username.trim(),
        body.email.as_deref(),
        &body.password,
        false,
    )
    .await?;

    let token = create_session_token(&state, &user)?;
    let cookie = session_cookie(&state, &token);

    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "user": UserView::from(&user) })),
    )
        .into_response())
}

async fn me(AuthUser(user): AuthUser) -> Json<Value> {
    Json(json!({ "user": UserView::from(&user) }))
}

#[derive(Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<Json<Value>> {
    if !verify_password(&body.current_password, &user.password_hash) {
        return Err(AppError::unauthorized("当前密码不正确"));
    }
    user_service::set_password(&state.pool, user.id, &body.new_password).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

async fn list_tokens(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Value>> {
    let tokens = token_service::list_tokens(&state.pool, user.id).await?;
    Ok(Json(json!({ "tokens": token_service::to_views(&tokens) })))
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: String,
    #[serde(default)]
    ttl_days: Option<i64>,
}

async fn create_token(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateTokenRequest>,
) -> AppResult<Response> {
    let (token, plaintext) =
        token_service::create_token(&state.pool, user.id, &body.name, "web", body.ttl_days).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": token.id,
            "name": token.name,
            "token": plaintext,
            "expires_at": token.expires_at,
        })),
    )
        .into_response())
}

async fn revoke_token(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    token_service::revoke_token(&state.pool, user.id, id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Packages
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PackageListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn list_packages(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Query(query): Query<PackageListQuery>,
) -> AppResult<Json<Value>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);

    let (packages, total) =
        pkg_service::list_packages(&state, query.q.as_deref(), limit, offset).await?;
    Ok(Json(json!({ "packages": packages, "total": total })))
}

async fn package_detail(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Path(name): Path<String>,
) -> AppResult<Json<Value>> {
    let name = decode_name(&name)?;
    let package = pkg_service::find_package(&state.pool, &name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;

    let versions = sqlx::query_as::<_, Version>(
        "SELECT * FROM versions WHERE package_id = ? ORDER BY id DESC",
    )
    .bind(package.id)
    .fetch_all(&state.pool)
    .await?;

    let latest: Option<(String,)> =
        sqlx::query_as("SELECT version FROM dist_tags WHERE package_id = ? AND tag = 'latest'")
            .bind(package.id)
            .fetch_optional(&state.pool)
            .await?;

    let owner = match package.owner_id {
        Some(id) => auth::find_user_by_id(&state.pool, id)
            .await?
            .map(|u| u.username),
        None => None,
    };

    let version_views: Vec<Value> = versions
        .iter()
        .map(|v| {
            json!({
                "version": v.version,
                "size": v.size,
                "downloads": v.downloads,
                "shasum": v.shasum,
                "integrity": v.integrity,
                "tarball_name": v.tarball_name,
                "created_at": v.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "name": package.name,
        "description": package.description,
        "license": package.license,
        "homepage": package.homepage,
        "owner": owner,
        "latest": latest.map(|l| l.0),
        "downloads": package.downloads,
        "created_at": package.created_at,
        "updated_at": package.updated_at,
        "versions": version_views,
    })))
}

#[derive(Deserialize)]
struct DeletePackageQuery {
    #[serde(default)]
    version: Option<String>,
}

async fn delete_package(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
    Query(query): Query<DeletePackageQuery>,
) -> AppResult<Json<Value>> {
    let name = decode_name(&name)?;
    match query.version {
        Some(version) if !version.is_empty() => {
            pkg_service::delete_version(&state, &user, &name, &version).await?;
        }
        _ => {
            pkg_service::delete_package(&state, &user, &name).await?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Users (admin)
// ---------------------------------------------------------------------------

async fn list_users(State(state): State<AppState>, _admin: AdminUser) -> AppResult<Json<Value>> {
    let users = sqlx::query_as::<_, crate::models::User>("SELECT * FROM users ORDER BY id ASC")
        .fetch_all(&state.pool)
        .await?;
    let views: Vec<UserView> = users.iter().map(UserView::from).collect();
    Ok(Json(json!({ "users": views })))
}

#[derive(Deserialize)]
struct CreateUserRequest {
    username: String,
    #[serde(default)]
    email: Option<String>,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

async fn create_user(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<CreateUserRequest>,
) -> AppResult<Response> {
    let user = user_service::create_user(
        &state.pool,
        body.username.trim(),
        body.email.as_deref(),
        &body.password,
        body.is_admin,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user": UserView::from(&user) })),
    )
        .into_response())
}

async fn delete_user(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    if id == admin.id {
        return Err(AppError::bad_request("不能删除当前登录的账号"));
    }
    let target = auth::find_user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("user not found"))?;

    if target.is_admin() {
        let (admins,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&state.pool)
            .await?;
        if admins <= 1 {
            return Err(AppError::bad_request("至少要保留一个管理员"));
        }
    }

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ResetPasswordRequest {
    password: String,
}

async fn reset_password(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
    Json(body): Json<ResetPasswordRequest>,
) -> AppResult<Json<Value>> {
    auth::find_user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("user not found"))?;
    user_service::set_password(&state.pool, id, &body.password).await?;
    // Force re-authentication with the new password.
    token_service::revoke_all(&state.pool, id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// CLI login handshake
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CliAuthorizeRequest {
    session_id: String,
}

/// Called by the dashboard after the user clicks "authorise" on a pending
/// `npm login` request. Mints a token and parks it for the CLI to collect.
async fn cli_authorize(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CliAuthorizeRequest>,
) -> AppResult<Json<Value>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, status FROM login_sessions WHERE id = ?")
            .bind(&body.session_id)
            .fetch_optional(&state.pool)
            .await?;

    let Some((_, status)) = row else {
        return Err(AppError::not_found("登录会话不存在或已过期"));
    };
    if status != "pending" {
        return Err(AppError::conflict("该登录会话已被处理"));
    }

    let (_, plaintext) =
        token_service::create_token(&state.pool, user.id, "npm CLI", "cli", Some(90)).await?;

    sqlx::query("UPDATE login_sessions SET user_id = ?, token = ?, status = 'done' WHERE id = ?")
        .bind(user.id)
        .bind(&plaintext)
        .bind(&body.session_id)
        .execute(&state.pool)
        .await?;

    Ok(Json(json!({ "ok": true })))
}

/// Create a pending login session (used by `routes::npm::login_web_start`).
pub async fn open_login_session(state: &AppState, hostname: Option<String>) -> AppResult<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let expires = rfc3339_in_minutes(10);
    sqlx::query(
        "INSERT INTO login_sessions (id, hostname, status, created_at, expires_at)
         VALUES (?, ?, 'pending', ?, ?)",
    )
    .bind(&id)
    .bind(hostname)
    .bind(now_rfc3339())
    .bind(&expires)
    .execute(&state.pool)
    .await?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn decode_name(raw: &str) -> AppResult<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|s| s.trim_start_matches('/').to_string())
        .map_err(|_| AppError::bad_request("invalid package name encoding"))
}

fn session_cookie(state: &AppState, token: &str) -> String {
    let secure = if state.config.registry_base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let max_age = state.config.session_ttl_minutes * 60;
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}
