//! The registry protocol spoken by the npm CLI.
//!
//! Package paths are not enumerated as routes (scoped names contain a slash, so
//! `/{package}/-/`-style patterns are ambiguous). Instead every `/-/`-prefixed
//! endpoint is registered explicitly and everything else is handled by
//! [`dispatch`], which parses the path the same way the registry spec describes.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::io::ReaderStream;

use crate::auth::{AuthUser, MaybeAuthUser};
use crate::error::{AppError, AppResult};
use crate::service::{packages as pkg_service, tokens as token_service, users as user_service};
use crate::state::AppState;
use crate::util::is_expired;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/-/ping", get(ping))
        .route("/-/whoami", get(whoami))
        .route("/-/user/{*rest}", put(user_login))
        .route("/-/v1/login", post(login_web_start))
        .route("/-/v1/login/{id}", get(login_web_poll))
        .route("/-/v1/search", get(search))
        .route(
            "/-/npm/v1/tokens",
            get(cli_list_tokens).post(cli_create_token),
        )
        .route("/-/npm/v1/tokens/token/{key}", delete(cli_revoke_token))
        // npm >= 7 talks to the dedicated dist-tags API rather than PUT-ing the
        // whole packument back.
        .route(
            "/-/package/{*rest}",
            get(dist_tags_list)
                .put(dist_tags_set)
                .delete(dist_tags_remove),
        )
        // `npm install` runs an audit; a permissive stub keeps the CLI quiet.
        .route("/-/npm/v1/security/audits/quick", post(audit_quick))
        .route("/-/npm/v1/security/advisories/bulk", post(advisories_bulk))
}

// ---------------------------------------------------------------------------
// Trivial endpoints
// ---------------------------------------------------------------------------

async fn ping() -> Json<Value> {
    Json(json!({}))
}

async fn whoami(AuthUser(user): AuthUser) -> Json<Value> {
    Json(json!({ "username": user.username }))
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// `PUT /-/user/org.couchdb.user:<name>` — legacy `npm login` / `npm adduser`.
async fn user_login(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    body: Bytes,
) -> AppResult<Response> {
    let decoded = percent_decode_str(&rest)
        .decode_utf8()
        .map_err(|_| AppError::bad_request("invalid user path"))?;

    // Path may be `org.couchdb.user:alice` or `org.couchdb.user:alice/-rev/1-abc`.
    let first = decoded.split('/').next().unwrap_or("").to_string();
    let username = first
        .rsplit(':')
        .next()
        .unwrap_or(&first)
        .trim()
        .to_string();
    if username.is_empty() {
        return Err(AppError::bad_request("missing username"));
    }

    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| AppError::bad_request(format!("invalid login payload: {e}")))?
    };
    let password = payload
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let email = payload.get("email").and_then(Value::as_str);

    if password.is_empty() {
        return Err(AppError::bad_request("password is required"));
    }

    let existing = crate::auth::find_user_by_name(&state.pool, &username).await?;

    let (user, created) = match existing {
        Some(user) => {
            if !user.is_active() {
                return Err(AppError::forbidden("account is disabled"));
            }
            if !crate::auth::verify_password(password, &user.password_hash) {
                return Err(AppError::unauthorized("incorrect username or password"));
            }
            (user, false)
        }
        None => {
            if !state.config.allow_registration {
                return Err(AppError::forbidden(
                    "self-registration is disabled on this registry",
                ));
            }
            let user =
                user_service::create_user(&state.pool, &username, email, password, false).await?;
            (user, true)
        }
    };

    let (_, plaintext) =
        token_service::create_token(&state.pool, user.id, "npm CLI", "cli", Some(90)).await?;

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    Ok((status, Json(json!({ "ok": true, "token": plaintext }))).into_response())
}

/// `POST /-/v1/login` — the browser-based `npm login` handshake.
async fn login_web_start(State(state): State<AppState>, body: Bytes) -> AppResult<Json<Value>> {
    let hostname = serde_json::from_slice::<Value>(&body).ok().and_then(|v| {
        v.get("hostname")
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    let id = crate::routes::api::open_login_session(&state, hostname).await?;
    let base = state.config.registry_base_url.trim_end_matches('/');

    Ok(Json(json!({
        "loginUrl": format!("{base}/login?cli={id}"),
        "doneUrl": format!("{base}/-/v1/login/{id}"),
    })))
}

/// `GET /-/v1/login/{id}` — polled by the CLI until the user authorises.
async fn login_web_poll(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as("SELECT status, token, expires_at FROM login_sessions WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;

    let Some((status, token, expires_at)) = row else {
        return Err(AppError::not_found("login session not found"));
    };
    if is_expired(&expires_at) {
        let _ = sqlx::query("DELETE FROM login_sessions WHERE id = ?")
            .bind(&id)
            .execute(&state.pool)
            .await;
        return Err(AppError::not_found("login session expired"));
    }

    if status == "done" {
        if let Some(token) = token {
            // Single-use: hand the token over, then forget it.
            let _ = sqlx::query("DELETE FROM login_sessions WHERE id = ?")
                .bind(&id)
                .execute(&state.pool)
                .await;
            return Ok((StatusCode::OK, Json(json!({ "token": token }))).into_response());
        }
    }

    Ok((StatusCode::ACCEPTED, Json(json!({}))).into_response())
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    from: Option<i64>,
}

async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Value>> {
    let text = query.text.unwrap_or_default();
    let size = query.size.unwrap_or(20).clamp(1, 250);
    let from = query.from.unwrap_or(0).max(0);
    Ok(Json(pkg_service::search(&state, &text, size, from).await?))
}

// ---------------------------------------------------------------------------
// CLI token management (`npm token list/create/revoke`)
// ---------------------------------------------------------------------------

async fn cli_list_tokens(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Value>> {
    let tokens = token_service::list_tokens(&state.pool, user.id).await?;
    let objects: Vec<Value> = tokens
        .iter()
        .map(|t| {
            json!({
                "key": t.id.to_string(),
                "token": t.token_prefix,
                "created": t.created_at,
                "updated": t.last_used_at.clone().unwrap_or_else(|| t.created_at.clone()),
                "readonly": false,
                "cidr_whitelist": Value::Null,
            })
        })
        .collect();
    Ok(Json(
        json!({ "objects": objects, "total": objects.len(), "urls": {} }),
    ))
}

#[derive(Deserialize)]
struct CliTokenCreate {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

async fn cli_create_token(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    body: Bytes,
) -> AppResult<Response> {
    let payload: CliTokenCreate = serde_json::from_slice(&body).unwrap_or(CliTokenCreate {
        password: None,
        name: None,
    });

    // `npm token create` asks for the password; when supplied, verify it.
    if let Some(password) = payload.password.as_deref() {
        if !password.is_empty() && !crate::auth::verify_password(password, &user.password_hash) {
            return Err(AppError::unauthorized("incorrect password"));
        }
    }

    let name = payload.name.unwrap_or_else(|| "npm token".to_string());
    let (token, plaintext) =
        token_service::create_token(&state.pool, user.id, &name, "cli", Some(90)).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key": token.id.to_string(),
            "token": plaintext,
            "created": token.created_at,
            "readonly": false,
            "cidr_whitelist": Value::Null,
        })),
    )
        .into_response())
}

async fn cli_revoke_token(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(key): Path<String>,
) -> AppResult<Json<Value>> {
    let id: i64 = key
        .parse()
        .map_err(|_| AppError::bad_request("invalid token key"))?;
    token_service::revoke_token(&state.pool, user.id, id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// dist-tags API (`/-/package/:name/dist-tags[/:tag]`)
// ---------------------------------------------------------------------------

/// Split `@scope/name/dist-tags[/tag]` into the package name and optional tag.
fn parse_dist_tag_path(rest: &str) -> AppResult<(String, Option<String>)> {
    const MARKER: &str = "/dist-tags";
    let idx = rest
        .find(MARKER)
        .ok_or_else(|| AppError::not_found("not found"))?;

    let name = &rest[..idx];
    if crate::storage::validate_package_name(name).is_err() {
        return Err(AppError::not_found("not found"));
    }

    let tail = &rest[idx + MARKER.len()..];
    let tag = match tail.strip_prefix('/') {
        Some(tag) if !tag.is_empty() => Some(tag.to_string()),
        Some(_) => return Err(AppError::not_found("not found")),
        None if tail.is_empty() => None,
        None => return Err(AppError::not_found("not found")),
    };

    Ok((name.to_string(), tag))
}

async fn dist_tags_list(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> AppResult<Json<Value>> {
    let (name, tag) = parse_dist_tag_path(&rest)?;
    let tags = pkg_service::list_dist_tags(&state, &name).await?;

    match tag {
        Some(tag) => {
            let version = tags.get(&tag).cloned().unwrap_or(Value::Null);
            Ok(Json(version))
        }
        None => Ok(Json(tags)),
    }
}

async fn dist_tags_set(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(rest): Path<String>,
    body: Bytes,
) -> AppResult<Response> {
    let (name, tag) = parse_dist_tag_path(&rest)?;
    let Some(tag) = tag else {
        return Err(AppError::bad_request("a dist-tag name is required"));
    };
    pkg_service::validate_tag(&tag)?;

    // npm sends the target version as a bare JSON string.
    let version = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::String(v)) => v,
        Ok(other) => other
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Err(_) => String::from_utf8_lossy(&body).trim().to_string(),
    };
    if version.is_empty() {
        return Err(AppError::bad_request("a version is required"));
    }

    pkg_service::set_dist_tag(&state, &user, &name, &tag, &version).await?;
    Ok(Json(json!({ "ok": true })).into_response())
}

async fn dist_tags_remove(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(rest): Path<String>,
) -> AppResult<Json<Value>> {
    let (name, tag) = parse_dist_tag_path(&rest)?;
    let Some(tag) = tag else {
        return Err(AppError::bad_request("a dist-tag name is required"));
    };
    pkg_service::remove_dist_tag(&state, &user, &name, &tag).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Audit stubs
// ---------------------------------------------------------------------------

/// We do not proxy the public advisory database. Returning an empty, well-formed
/// report keeps `npm install` / `npm audit` from failing on a private registry.
async fn audit_quick() -> Json<Value> {
    Json(json!({
        "actions": [],
        "advisories": {},
        "muted": [],
        "metadata": {
            "vulnerabilities": {
                "info": 0, "low": 0, "moderate": 0, "high": 0, "critical": 0, "total": 0
            },
            "dependencies": 0,
            "devDependencies": 0,
            "optionalDependencies": 0,
            "totalDependencies": 0
        },
        "runId": "npm-registry-rust",
    }))
}

async fn advisories_bulk() -> Json<Value> {
    Json(json!({}))
}

// ---------------------------------------------------------------------------
// Package paths (catch-all fallback)
// ---------------------------------------------------------------------------

/// Everything that is not one of the endpoints above is a package path.
pub async fn dispatch(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> AppResult<Response> {
    let decoded = percent_decode_str(uri.path())
        .decode_utf8()
        .map_err(|_| AppError::bad_request("invalid path encoding"))?
        .to_string();

    let mut response = match method {
        Method::GET | Method::HEAD => get_route(&state, &decoded).await?,
        Method::PUT => put_route(&state, auth.0, &decoded, body).await?,
        Method::DELETE => delete_route(&state, auth.0, &decoded, body).await?,
        _ => return Err(AppError::bad_request("unsupported method")),
    };

    // HEAD responses must not carry a body.
    if method == Method::HEAD {
        *response.body_mut() = Body::empty();
    }

    Ok(response)
}

#[derive(Debug, PartialEq)]
enum PackagePath {
    /// `GET /foo`, `GET /@scope/foo`
    Packument(String),
    /// `GET /foo/1.2.3`
    Version(String, String),
    /// `GET /foo/-/foo-1.2.3.tgz`
    Tarball(String, String),
    /// `PUT|DELETE /foo/-rev/1-abc`
    Revision(String),
    Invalid,
}

fn parse_path(path: &str) -> PackagePath {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return PackagePath::Invalid;
    }

    if let Some(idx) = path.find("/-rev/") {
        let package = &path[..idx];
        return if crate::storage::validate_package_name(package).is_ok() {
            PackagePath::Revision(package.to_string())
        } else {
            PackagePath::Invalid
        };
    }

    if let Some(idx) = path.find("/-/") {
        let package = &path[..idx];
        let filename = &path[idx + 3..];
        return match (
            crate::storage::validate_package_name(package),
            crate::storage::validate_filename(filename),
        ) {
            (Ok(_), Ok(_)) => PackagePath::Tarball(package.to_string(), filename.to_string()),
            _ => PackagePath::Invalid,
        };
    }

    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        [name] if !name.starts_with('@') => {
            if crate::storage::validate_package_name(name).is_ok() {
                PackagePath::Packument(name.to_string())
            } else {
                PackagePath::Invalid
            }
        }
        [scope, name] if scope.starts_with('@') => {
            let full = format!("{scope}/{name}");
            if crate::storage::validate_package_name(&full).is_ok() {
                PackagePath::Packument(full)
            } else {
                PackagePath::Invalid
            }
        }
        [name, version] if !name.starts_with('@') => {
            if crate::storage::validate_package_name(name).is_ok() {
                PackagePath::Version(name.to_string(), version.to_string())
            } else {
                PackagePath::Invalid
            }
        }
        [scope, name, version] if scope.starts_with('@') => {
            let full = format!("{scope}/{name}");
            if crate::storage::validate_package_name(&full).is_ok() {
                PackagePath::Version(full, version.to_string())
            } else {
                PackagePath::Invalid
            }
        }
        _ => PackagePath::Invalid,
    }
}

async fn get_route(state: &AppState, path: &str) -> AppResult<Response> {
    match parse_path(path) {
        PackagePath::Packument(name) => packument_response(state, &name).await,
        PackagePath::Version(name, version) => version_response(state, &name, &version).await,
        PackagePath::Tarball(name, filename) => serve_tarball(state, &name, &filename).await,
        // Some clients re-GET a revision; serve the packument.
        PackagePath::Revision(name) => packument_response(state, &name).await,
        PackagePath::Invalid => Err(AppError::not_found("not found")),
    }
}

/// Local packages are assembled from the database; anything else is relayed from
/// an upstream, streaming, without ever being buffered here.
async fn packument_response(state: &AppState, name: &str) -> AppResult<Response> {
    match pkg_service::resolve_packument(state, name).await? {
        pkg_service::PackumentResponse::Local(doc) => Ok(Json(doc).into_response()),
        pkg_service::PackumentResponse::Relay(relay) => Ok(relay.into_response()),
    }
}

async fn version_response(state: &AppState, name: &str, version: &str) -> AppResult<Response> {
    match pkg_service::resolve_version(state, name, version).await? {
        pkg_service::PackumentResponse::Local(doc) => Ok(Json(doc).into_response()),
        pkg_service::PackumentResponse::Relay(relay) => Ok(relay.into_response()),
    }
}

async fn put_route(
    state: &AppState,
    user: Option<crate::models::User>,
    path: &str,
    body: Bytes,
) -> AppResult<Response> {
    let user = user.ok_or_else(|| {
        AppError::unauthorized("authentication required: run `npm login` or configure a token")
    })?;

    match parse_path(path) {
        PackagePath::Packument(name) | PackagePath::Revision(name) => {
            let payload: Value = serde_json::from_slice(&body)
                .map_err(|e| AppError::bad_request(format!("invalid packument JSON: {e}")))?;
            let result = pkg_service::upsert_packument(state, &user, &name, payload).await?;
            Ok((StatusCode::CREATED, Json(result)).into_response())
        }
        _ => Err(AppError::bad_request("unsupported publish target")),
    }
}

async fn delete_route(
    state: &AppState,
    user: Option<crate::models::User>,
    path: &str,
    body: Bytes,
) -> AppResult<Response> {
    let user = user.ok_or_else(|| AppError::unauthorized("authentication required"))?;

    let name = match parse_path(path) {
        PackagePath::Revision(name) | PackagePath::Packument(name) => name,
        _ => return Err(AppError::bad_request("unsupported unpublish target")),
    };

    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({}))
    };

    let result = pkg_service::unpublish(state, &user, &name, payload).await?;
    Ok(Json(result).into_response())
}

async fn serve_tarball(state: &AppState, name: &str, filename: &str) -> AppResult<Response> {
    let Some((package, version)) = pkg_service::find_tarball(state, name, filename).await? else {
        return Err(AppError::not_found(format!(
            "{name}/-/{filename} not found"
        )));
    };

    let full = state
        .storage
        .abs(&version.tarball_path)
        .map_err(|e| AppError::Internal(e.context("invalid stored tarball path")))?;

    let file = tokio::fs::File::open(&full).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "tarball '{}' is missing from storage: {e}",
            version.tarball_path
        ))
    })?;

    let size = file.metadata().await.map(|m| m.len()).unwrap_or_default();

    pkg_service::record_download(state, package.id, version.id);

    // Streamed rather than buffered: cached upstream packages can be tens of
    // megabytes, and this process runs under a memory cap.
    let body = Body::from_stream(ReaderStream::new(file));

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ETAG, format!("\"{}\"", version.shasum))
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(body)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to build response: {e}")))?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_paths() {
        assert_eq!(
            parse_path("/lodash"),
            PackagePath::Packument("lodash".into())
        );
        assert_eq!(
            parse_path("/@acme/widgets"),
            PackagePath::Packument("@acme/widgets".into())
        );
        assert_eq!(
            parse_path("/lodash/4.17.21"),
            PackagePath::Version("lodash".into(), "4.17.21".into())
        );
        assert_eq!(
            parse_path("/@acme/widgets/1.0.0"),
            PackagePath::Version("@acme/widgets".into(), "1.0.0".into())
        );
        assert_eq!(
            parse_path("/lodash/-/lodash-4.17.21.tgz"),
            PackagePath::Tarball("lodash".into(), "lodash-4.17.21.tgz".into())
        );
        assert_eq!(
            parse_path("/lodash/-rev/1-abc"),
            PackagePath::Revision("lodash".into())
        );
    }

    #[test]
    fn rejects_nonsense() {
        assert_eq!(parse_path("/"), PackagePath::Invalid);
        assert_eq!(parse_path("/../etc"), PackagePath::Invalid);
    }
}
