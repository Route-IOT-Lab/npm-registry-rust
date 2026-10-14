//! Package publishing, packument assembly, unpublishing and search.
//!
//! Everything the npm registry protocol needs on the data side. Handlers in
//! `routes::npm` only translate HTTP <-> these functions.

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha1::{Digest as _, Sha1};
use sha2::Sha512;
use sqlx::{SqlitePool, Transaction};

use crate::error::AppError;
use crate::models::{Package, User, Version};
use crate::state::AppState;
use crate::storage::validate_package_name;
use crate::upstream::UpstreamLookup;
use crate::util::now_rfc3339;

const DEFAULT_TAG: &str = "latest";

// ---------------------------------------------------------------------------
// Permission model
// ---------------------------------------------------------------------------

/// First publisher owns the package; afterwards only the owner, listed
/// maintainers, and administrators may push to it.
pub async fn ensure_can_publish(
    state: &AppState,
    user: &User,
    package: Option<&Package>,
) -> Result<(), AppError> {
    let Some(pkg) = package else {
        return Ok(()); // new package: whoever publishes first owns it
    };
    if user.is_admin() || pkg.owner_id == Some(user.id) {
        return Ok(());
    }
    let is_maintainer: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM maintainers WHERE package_id = ? AND user_id = ?")
            .bind(pkg.id)
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?;
    if is_maintainer.is_some() {
        return Ok(());
    }
    Err(AppError::forbidden(format!(
        "you do not have permission to publish to '{}'",
        pkg.name
    )))
}

async fn ensure_can_manage(_state: &AppState, user: &User, pkg: &Package) -> Result<(), AppError> {
    if user.is_admin() || pkg.owner_id == Some(user.id) {
        return Ok(());
    }
    Err(AppError::forbidden(format!(
        "you do not have permission to modify '{}'",
        pkg.name
    )))
}

// ---------------------------------------------------------------------------
// Publish / upsert
// ---------------------------------------------------------------------------

/// Handle a `PUT /:package` body: store attachments, persist version manifests
/// and merge dist-tags. Also serves `dist-tag` updates, which PUT the packument
/// back with no attachments.
pub async fn upsert_packument(
    state: &AppState,
    user: &User,
    name_hint: &str,
    body: Value,
) -> Result<Value, AppError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(name_hint)
        .to_string();

    validate_package_name(&name).map_err(AppError::BadRequest)?;

    let versions = body
        .get("versions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if versions.is_empty() {
        return Err(AppError::bad_request("packument contains no versions"));
    }

    let existing = find_package(&state.pool, &name).await?;
    ensure_can_publish(state, user, existing.as_ref()).await?;

    // --- 1. Decode every attachment up-front (before we touch the database) ---
    let attachments = decode_attachments(&body)?;

    let mut prepared: Vec<PreparedVersion> = Vec::new();
    for (version, manifest) in &versions {
        semver::Version::parse(version)
            .map_err(|_| AppError::bad_request(format!("invalid semver version '{version}'")))?;

        let filename = tarball_filename(&name, version, manifest);
        let bytes = attachments
            .get(&filename)
            .or_else(|| {
                if attachments.len() == 1 {
                    attachments.values().next()
                } else {
                    None
                }
            })
            .cloned();

        match bytes {
            Some(bytes) => {
                let rel_path = crate::storage::Storage::rel_path(&name, &filename)?;
                let shasum = sha1_hex(&bytes);
                let integrity = sha512_integrity(&bytes);
                let size = bytes.len() as i64;
                state.storage.write(&rel_path, &bytes)?;

                let mut manifest = manifest.clone();
                rewrite_dist(&mut manifest, state, &name, &filename, &shasum, &integrity);
                prepared.push(PreparedVersion {
                    version: version.clone(),
                    manifest,
                    filename,
                    rel_path,
                    shasum,
                    integrity,
                    size,
                });
            }
            None => {
                // No attachment: this is a metadata-only PUT (e.g. `npm dist-tag`).
                // The version must already exist, otherwise the packument is broken.
                let known = sqlx::query_as::<_, Version>(
                    "SELECT * FROM versions WHERE package_id = (SELECT id FROM packages WHERE name = ?) AND version = ?",
                )
                .bind(&name)
                .bind(version)
                .fetch_optional(&state.pool)
                .await?;

                match known {
                    Some(existing_version) => {
                        let mut manifest = manifest.clone();
                        rewrite_dist(
                            &mut manifest,
                            state,
                            &name,
                            &existing_version.tarball_name,
                            &existing_version.shasum,
                            &existing_version.integrity,
                        );
                        prepared.push(PreparedVersion {
                            version: version.clone(),
                            manifest,
                            filename: existing_version.tarball_name,
                            rel_path: existing_version.tarball_path,
                            shasum: existing_version.shasum,
                            integrity: existing_version.integrity,
                            size: existing_version.size,
                        });
                    }
                    None => {
                        return Err(AppError::bad_request(format!(
                            "missing tarball attachment for new version '{version}'"
                        )))
                    }
                }
            }
        }
    }

    // --- 2. Persist everything inside one transaction ---
    let now = now_rfc3339();
    // `BEGIN IMMEDIATE`, not the deferred default: two deferred transactions that
    // both start as readers and then try to write deadlock, and SQLite reports
    // `database is locked` immediately rather than waiting out `busy_timeout`.
    // Taking the write lock up front makes concurrent publishers queue politely.
    let mut tx = state.pool.begin_with("BEGIN IMMEDIATE").await?;

    let package_id = match existing {
        Some(pkg) => {
            sqlx::query(
                "UPDATE packages
                    SET description = COALESCE(?, description),
                        readme      = COALESCE(?, readme),
                        homepage    = COALESCE(?, homepage),
                        license     = COALESCE(?, license),
                        keywords    = COALESCE(?, keywords),
                        updated_at  = ?
                  WHERE id = ?",
            )
            .bind(str_field(&body, "description"))
            .bind(str_field(&body, "readme"))
            .bind(str_field(&body, "homepage"))
            .bind(str_field(&body, "license"))
            .bind(keywords_field(&body))
            .bind(&now)
            .bind(pkg.id)
            .execute(&mut *tx)
            .await?;
            pkg.id
        }
        None => {
            let result = sqlx::query(
                "INSERT INTO packages
                    (name, description, readme, homepage, license, keywords, owner_id, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&name)
            .bind(str_field(&body, "description"))
            .bind(str_field(&body, "readme"))
            .bind(str_field(&body, "homepage"))
            .bind(str_field(&body, "license"))
            .bind(keywords_field(&body))
            .bind(user.id)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            let id = result.last_insert_rowid();
            sqlx::query("INSERT OR IGNORE INTO maintainers (package_id, user_id) VALUES (?, ?)")
                .bind(id)
                .bind(user.id)
                .execute(&mut *tx)
                .await?;
            id
        }
    };

    for prepared_version in &prepared {
        sqlx::query(
            "INSERT INTO versions
                (package_id, version, manifest, tarball_name, tarball_path, shasum, integrity, size, published_by, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (package_id, version) DO UPDATE SET
                manifest     = excluded.manifest,
                tarball_name = excluded.tarball_name,
                tarball_path = excluded.tarball_path,
                shasum       = excluded.shasum,
                integrity    = excluded.integrity,
                size         = excluded.size",
        )
        .bind(package_id)
        .bind(&prepared_version.version)
        .bind(prepared_version.manifest.to_string())
        .bind(&prepared_version.filename)
        .bind(&prepared_version.rel_path)
        .bind(&prepared_version.shasum)
        .bind(&prepared_version.integrity)
        .bind(prepared_version.size)
        .bind(user.id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }

    // --- 3. dist-tags ---
    if let Some(tags) = body.get("dist-tags").and_then(Value::as_object) {
        for (tag, target) in tags {
            let Some(target) = target.as_str() else {
                continue;
            };
            upsert_dist_tag(&mut tx, package_id, tag, target).await?;
        }
    }
    if !has_tag(&mut tx, package_id, DEFAULT_TAG).await? {
        let sorted = highest_version(versions.keys().cloned().collect())?;
        upsert_dist_tag(&mut tx, package_id, DEFAULT_TAG, &sorted).await?;
    }

    tx.commit().await?;

    Ok(json!({ "ok": true, "id": name, "rev": "1-0" }))
}

struct PreparedVersion {
    version: String,
    manifest: Value,
    filename: String,
    rel_path: String,
    shasum: String,
    integrity: String,
    size: i64,
}

// ---------------------------------------------------------------------------
// Packument
// ---------------------------------------------------------------------------

/// Assemble the full registry document for a package.
pub async fn build_packument(state: &AppState, name: &str) -> Result<Value, AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;

    let versions =
        sqlx::query_as::<_, Version>("SELECT * FROM versions WHERE package_id = ? ORDER BY id ASC")
            .bind(pkg.id)
            .fetch_all(&state.pool)
            .await?;

    if versions.is_empty() {
        return Err(AppError::not_found(format!(
            "package '{name}' has no versions"
        )));
    }

    let tags =
        sqlx::query_as::<_, crate::models::DistTag>("SELECT * FROM dist_tags WHERE package_id = ?")
            .bind(pkg.id)
            .fetch_all(&state.pool)
            .await?;

    let owner = match pkg.owner_id {
        Some(id) => crate::auth::find_user_by_id(&state.pool, id).await?,
        None => None,
    };

    let mut versions_map = Map::new();
    let mut time_map = Map::new();
    time_map.insert("created".into(), json!(pkg.created_at));
    time_map.insert("modified".into(), json!(pkg.updated_at));

    for v in &versions {
        let mut manifest = serde_json::from_str::<Value>(&v.manifest).unwrap_or_else(|_| json!({}));
        rewrite_dist(
            &mut manifest,
            state,
            &pkg.name,
            &v.tarball_name,
            &v.shasum,
            &v.integrity,
        );
        versions_map.insert(v.version.clone(), manifest);
        time_map.insert(v.version.clone(), json!(v.created_at));
    }

    let mut tags_map = Map::new();
    for t in &tags {
        tags_map.insert(t.tag.clone(), json!(t.version));
    }

    let mut doc = Map::new();
    doc.insert("_id".into(), json!(pkg.name));
    doc.insert("name".into(), json!(pkg.name));
    doc.insert("dist-tags".into(), Value::Object(tags_map));
    doc.insert("versions".into(), Value::Object(versions_map));
    doc.insert("time".into(), Value::Object(time_map));
    doc.insert("_rev".into(), json!("1-0"));
    doc.insert("_attachments".into(), json!({}));
    if let Some(desc) = &pkg.description {
        doc.insert("description".into(), json!(desc));
    }
    if let Some(readme) = &pkg.readme {
        doc.insert("readme".into(), json!(readme));
    }
    if let Some(homepage) = &pkg.homepage {
        doc.insert("homepage".into(), json!(homepage));
    }
    if let Some(license) = &pkg.license {
        doc.insert("license".into(), json!(license));
    }
    if let Some(keywords) = parse_keywords(pkg.keywords.as_deref()) {
        doc.insert("keywords".into(), keywords);
    }
    if let Some(owner) = &owner {
        doc.insert(
            "maintainers".into(),
            json!([{
                "name": owner.username,
                "email": owner.email,
            }]),
        );
    }

    Ok(Value::Object(doc))
}

/// Fetch a single version manifest (used by `GET /:package/:version`).
pub async fn build_version_manifest(
    state: &AppState,
    name: &str,
    version: &str,
) -> Result<Value, AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;

    let row =
        sqlx::query_as::<_, Version>("SELECT * FROM versions WHERE package_id = ? AND version = ?")
            .bind(pkg.id)
            .bind(version)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::not_found(format!("{name}@{version} not found")))?;

    let mut manifest = serde_json::from_str::<Value>(&row.manifest).unwrap_or_else(|_| json!({}));
    rewrite_dist(
        &mut manifest,
        state,
        &pkg.name,
        &row.tarball_name,
        &row.shasum,
        &row.integrity,
    );
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Upstream resolution
// ---------------------------------------------------------------------------

/// What a packument request should return.
pub enum PackumentResponse {
    /// Assembled from this registry's own data.
    Local(Value),
    /// Relayed straight from an upstream, byte for byte.
    Relay(crate::upstream::Relay),
}

/// Resolve a packument: local first, upstream chain only on a miss.
///
/// Local packages always win. A name that exists locally is never merged with
/// upstream content, so "what does this registry serve" stays predictable.
pub async fn resolve_packument(
    state: &AppState,
    name: &str,
) -> Result<PackumentResponse, AppError> {
    if find_package(&state.pool, name).await?.is_some() {
        return Ok(PackumentResponse::Local(
            build_packument(state, name).await?,
        ));
    }

    let Some(client) = &state.upstream else {
        return Err(AppError::not_found(format!("package '{name}' not found")));
    };

    match client.relay(name).await? {
        UpstreamLookup::Found(relay) => Ok(PackumentResponse::Relay(relay)),
        UpstreamLookup::NotFound => Err(AppError::not_found(format!(
            "package '{name}' was not found in this registry or on any upstream"
        ))),
    }
}

/// Same idea for `GET /:package/:version`.
pub async fn resolve_version(
    state: &AppState,
    name: &str,
    version: &str,
) -> Result<PackumentResponse, AppError> {
    if find_package(&state.pool, name).await?.is_some() {
        return Ok(PackumentResponse::Local(
            build_version_manifest(state, name, version).await?,
        ));
    }

    let Some(client) = &state.upstream else {
        return Err(AppError::not_found(format!("{name}@{version} not found")));
    };

    match client.relay(&format!("{name}/{version}")).await? {
        UpstreamLookup::Found(relay) => Ok(PackumentResponse::Relay(relay)),
        UpstreamLookup::NotFound => Err(AppError::not_found(format!(
            "{name}@{version} was not found in this registry or on any upstream"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Unpublish / delete
// ---------------------------------------------------------------------------

/// `DELETE /:package/-rev/:rev`. Removes the listed versions, or the whole
/// package when nothing is left / no version list is supplied.
pub async fn unpublish(
    state: &AppState,
    user: &User,
    name: &str,
    body: Value,
) -> Result<Value, AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;
    ensure_can_manage(state, user, &pkg).await?;

    let requested: Vec<String> = body
        .get("versions")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    if requested.is_empty() {
        // Full removal.
        delete_package_rows(state, &pkg).await?;
        return Ok(json!({ "ok": true }));
    }

    for version in &requested {
        delete_version_rows(state, pkg.id, version).await?;
    }

    // Nothing left -> drop the package entirely.
    let (remaining,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM versions WHERE package_id = ?")
        .bind(pkg.id)
        .fetch_one(&state.pool)
        .await?;
    if remaining == 0 {
        delete_package_rows(state, &pkg).await?;
    } else {
        repoint_latest(state, pkg.id).await?;
    }

    Ok(json!({ "ok": true }))
}

pub async fn delete_package(state: &AppState, user: &User, name: &str) -> Result<(), AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;
    ensure_can_manage(state, user, &pkg).await?;
    delete_package_rows(state, &pkg).await
}

pub async fn delete_version(
    state: &AppState,
    user: &User,
    name: &str,
    version: &str,
) -> Result<(), AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;
    ensure_can_manage(state, user, &pkg).await?;
    delete_version_rows(state, pkg.id, version).await?;

    let (remaining,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM versions WHERE package_id = ?")
        .bind(pkg.id)
        .fetch_one(&state.pool)
        .await?;
    if remaining == 0 {
        delete_package_rows(state, &pkg).await?;
    } else {
        repoint_latest(state, pkg.id).await?;
    }
    Ok(())
}

async fn delete_version_rows(
    state: &AppState,
    package_id: i64,
    version: &str,
) -> Result<(), AppError> {
    let row =
        sqlx::query_as::<_, Version>("SELECT * FROM versions WHERE package_id = ? AND version = ?")
            .bind(package_id)
            .bind(version)
            .fetch_optional(&state.pool)
            .await?;

    let Some(row) = row else {
        return Err(AppError::not_found(format!(
            "version '{version}' not found"
        )));
    };

    sqlx::query("DELETE FROM versions WHERE id = ?")
        .bind(row.id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM dist_tags WHERE package_id = ? AND version = ?")
        .bind(package_id)
        .bind(version)
        .execute(&state.pool)
        .await?;

    if let Err(e) = state.storage.remove(&row.tarball_path) {
        tracing::warn!(path = %row.tarball_path, error = %e, "failed to delete tarball");
    }
    Ok(())
}

async fn delete_package_rows(state: &AppState, pkg: &Package) -> Result<(), AppError> {
    sqlx::query("DELETE FROM packages WHERE id = ?")
        .bind(pkg.id)
        .execute(&state.pool)
        .await?;
    if let Err(e) = state.storage.remove_package(&pkg.name) {
        tracing::warn!(package = %pkg.name, error = %e, "failed to delete package directory");
    }
    Ok(())
}

/// Keep `latest` pointing at the highest remaining version.
async fn repoint_latest(state: &AppState, package_id: i64) -> Result<(), AppError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT version FROM versions WHERE package_id = ?")
        .bind(package_id)
        .fetch_all(&state.pool)
        .await?;
    let versions: Vec<String> = rows.into_iter().map(|r| r.0).collect();
    let Some(highest) = highest_version(versions).ok() else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version) VALUES (?, ?, ?)
         ON CONFLICT (package_id, tag) DO UPDATE SET version = excluded.version",
    )
    .bind(package_id)
    .bind(DEFAULT_TAG)
    .bind(&highest)
    .execute(&state.pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing & search
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PackageSummary {
    pub name: String,
    pub description: Option<String>,
    pub latest: Option<String>,
    pub versions: i64,
    pub downloads: i64,
    pub owner: Option<String>,
    pub license: Option<String>,
    pub updated_at: String,
    pub created_at: String,
}

pub async fn list_packages(
    state: &AppState,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PackageSummary>, i64), AppError> {
    let like = query.map(|q| format!("%{}%", q.to_lowercase()));

    let rows = sqlx::query_as::<_, PackageSummaryRow>(
        "SELECT p.id,
                p.name,
                p.description,
                p.license,
                p.downloads,
                p.created_at,
                p.updated_at,
                (SELECT u.username FROM users u WHERE u.id = p.owner_id) AS owner,
                (SELECT COUNT(*) FROM versions v WHERE v.package_id = p.id) AS version_count,
                (SELECT dt.version FROM dist_tags dt WHERE dt.package_id = p.id AND dt.tag = 'latest') AS latest
           FROM packages p
          WHERE (? IS NULL OR lower(p.name) LIKE ? OR lower(COALESCE(p.description,'')) LIKE ?)
          ORDER BY p.updated_at DESC
          LIMIT ? OFFSET ?",
    )
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM packages p
          WHERE (? IS NULL OR lower(p.name) LIKE ? OR lower(COALESCE(p.description,'')) LIKE ?)",
    )
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(like.as_deref())
    .fetch_one(&state.pool)
    .await?;

    let summaries = rows
        .into_iter()
        .map(|r| PackageSummary {
            name: r.name,
            description: r.description,
            latest: r.latest,
            versions: r.version_count,
            downloads: r.downloads,
            owner: r.owner,
            license: r.license,
            updated_at: r.updated_at,
            created_at: r.created_at,
        })
        .collect();

    Ok((summaries, total))
}

#[derive(sqlx::FromRow)]
struct PackageSummaryRow {
    #[allow(dead_code)]
    id: i64,
    name: String,
    description: Option<String>,
    license: Option<String>,
    downloads: i64,
    created_at: String,
    updated_at: String,
    owner: Option<String>,
    version_count: i64,
    latest: Option<String>,
}

/// `GET /-/v1/search` — npm's search API.
pub async fn search(state: &AppState, text: &str, size: i64, from: i64) -> Result<Value, AppError> {
    let (packages, total) = list_packages(state, Some(text), size, from).await?;

    let objects: Vec<Value> = packages
        .iter()
        .map(|p| {
            json!({
                "package": {
                    "name": p.name,
                    "version": p.latest,
                    "description": p.description,
                    "date": p.updated_at,
                    "links": {
                        "npm": format!("{}/{}", state.config.registry_base_url, p.name)
                    },
                    "publisher": p.owner.as_ref().map(|o| json!({ "username": o })),
                    "maintainers": p.owner.as_ref().map(|o| json!([{ "username": o }])),
                },
                "score": {
                    "final": 1.0,
                    "detail": { "quality": 1.0, "popularity": 0.0, "maintenance": 1.0 }
                },
                "searchScore": 1.0
            })
        })
        .collect();

    Ok(json!({
        "objects": objects,
        "total": total,
        "time": now_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------

pub fn record_download(state: &AppState, package_id: i64, version_id: i64) {
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE packages SET downloads = downloads + 1 WHERE id = ?")
            .bind(package_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("UPDATE versions SET downloads = downloads + 1 WHERE id = ?")
            .bind(version_id)
            .execute(&pool)
            .await;
    });
}

/// Look up the row backing a tarball request.
pub async fn find_tarball(
    state: &AppState,
    name: &str,
    filename: &str,
) -> Result<Option<(Package, Version)>, AppError> {
    let Some(pkg) = find_package(&state.pool, name).await? else {
        return Ok(None);
    };
    let version = sqlx::query_as::<_, Version>(
        "SELECT * FROM versions WHERE package_id = ? AND tarball_name = ?",
    )
    .bind(pkg.id)
    .bind(filename)
    .fetch_optional(&state.pool)
    .await?;
    Ok(version.map(|v| (pkg, v)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub async fn find_package(pool: &SqlitePool, name: &str) -> Result<Option<Package>, AppError> {
    let pkg = sqlx::query_as::<_, Package>("SELECT * FROM packages WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(pkg)
}

fn decode_attachments(
    body: &Value,
) -> Result<std::collections::HashMap<String, Vec<u8>>, AppError> {
    let mut out = std::collections::HashMap::new();
    let Some(attachments) = body.get("_attachments").and_then(Value::as_object) else {
        return Ok(out);
    };
    for (filename, att) in attachments {
        let Some(data) = att.get("data").and_then(Value::as_str) else {
            continue;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| {
                AppError::bad_request(format!("invalid base64 attachment '{filename}': {e}"))
            })?;
        out.insert(filename.clone(), bytes);
    }
    Ok(out)
}

fn tarball_filename(name: &str, version: &str, manifest: &Value) -> String {
    if let Some(url) = manifest
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(Value::as_str)
    {
        if let Some(base) = url.rsplit('/').next() {
            if base.ends_with(".tgz") {
                return base.to_string();
            }
        }
    }
    let bare = name.rsplit('/').next().unwrap_or(name);
    format!("{bare}-{version}.tgz")
}

fn rewrite_dist(
    manifest: &mut Value,
    state: &AppState,
    package: &str,
    filename: &str,
    shasum: &str,
    integrity: &str,
) {
    let tarball_url = format!(
        "{}/{}/-/{}",
        state.config.registry_base_url.trim_end_matches('/'),
        package,
        filename
    );
    let Some(obj) = manifest.as_object_mut() else {
        return;
    };
    let dist = obj.entry("dist".to_string()).or_insert_with(|| json!({}));
    if let Some(dist) = dist.as_object_mut() {
        dist.insert("tarball".into(), json!(tarball_url));
        dist.insert("shasum".into(), json!(shasum));
        dist.insert("integrity".into(), json!(integrity));
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn sha512_integrity(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut hasher = Sha512::new();
    hasher.update(bytes);
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    )
}

fn str_field(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

fn keywords_field(body: &Value) -> Option<String> {
    body.get("keywords").map(|k| k.to_string())
}

fn parse_keywords(raw: Option<&str>) -> Option<Value> {
    let raw = raw?;
    serde_json::from_str::<Value>(raw).ok()
}

fn highest_version(mut versions: Vec<String>) -> Result<String, AppError> {
    versions.sort_by(
        |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        },
    );
    versions
        .pop()
        .ok_or_else(|| AppError::bad_request("no valid versions to tag"))
}

async fn upsert_dist_tag(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    package_id: i64,
    tag: &str,
    version: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version) VALUES (?, ?, ?)
         ON CONFLICT (package_id, tag) DO UPDATE SET version = excluded.version",
    )
    .bind(package_id)
    .bind(tag)
    .bind(version)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn has_tag(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    package_id: i64,
    tag: &str,
) -> Result<bool, AppError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM dist_tags WHERE package_id = ? AND tag = ?")
            .bind(package_id)
            .bind(tag)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.is_some())
}

// ---------------------------------------------------------------------------
// dist-tags API (`/-/package/:name/dist-tags`)
// ---------------------------------------------------------------------------

/// Validate a dist-tag name. Tags must not be parseable as a semver range —
/// otherwise `npm install pkg@1.2.3` would become ambiguous.
pub fn validate_tag(tag: &str) -> Result<(), AppError> {
    let trimmed = tag.trim();
    if trimmed.is_empty() || trimmed.len() > 214 {
        return Err(AppError::bad_request("invalid dist-tag"));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(AppError::bad_request("dist-tag may not contain whitespace"));
    }
    if semver::Version::parse(trimmed).is_ok() {
        return Err(AppError::bad_request(
            "dist-tag may not be a valid semver version",
        ));
    }
    Ok(())
}

pub async fn list_dist_tags(state: &AppState, name: &str) -> Result<Value, AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;

    let rows =
        sqlx::query_as::<_, crate::models::DistTag>("SELECT * FROM dist_tags WHERE package_id = ?")
            .bind(pkg.id)
            .fetch_all(&state.pool)
            .await?;

    let mut map = Map::new();
    for row in rows {
        map.insert(row.tag, json!(row.version));
    }
    Ok(Value::Object(map))
}

pub async fn set_dist_tag(
    state: &AppState,
    user: &User,
    name: &str,
    tag: &str,
    version: &str,
) -> Result<(), AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;
    ensure_can_manage(state, user, &pkg).await?;

    let exists: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM versions WHERE package_id = ? AND version = ?")
            .bind(pkg.id)
            .bind(version)
            .fetch_optional(&state.pool)
            .await?;
    if exists.is_none() {
        return Err(AppError::not_found(format!(
            "{name}@{version} does not exist"
        )));
    }

    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version) VALUES (?, ?, ?)
         ON CONFLICT (package_id, tag) DO UPDATE SET version = excluded.version",
    )
    .bind(pkg.id)
    .bind(tag)
    .bind(version)
    .execute(&state.pool)
    .await?;

    touch_package(&state.pool, pkg.id).await?;
    Ok(())
}

pub async fn remove_dist_tag(
    state: &AppState,
    user: &User,
    name: &str,
    tag: &str,
) -> Result<(), AppError> {
    let pkg = find_package(&state.pool, name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("package '{name}' not found")))?;
    ensure_can_manage(state, user, &pkg).await?;

    let result = sqlx::query("DELETE FROM dist_tags WHERE package_id = ? AND tag = ?")
        .bind(pkg.id)
        .bind(tag)
        .execute(&state.pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::not_found(format!(
            "dist-tag '{tag}' does not exist"
        )));
    }

    touch_package(&state.pool, pkg.id).await?;
    Ok(())
}

async fn touch_package(pool: &SqlitePool, package_id: i64) -> Result<(), AppError> {
    sqlx::query("UPDATE packages SET updated_at = ? WHERE id = ?")
        .bind(now_rfc3339())
        .bind(package_id)
        .execute(pool)
        .await?;
    Ok(())
}
