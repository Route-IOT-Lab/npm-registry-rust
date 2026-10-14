//! Centralised, fail-fast configuration.
//!
//! Every knob comes from the environment and is validated **once** at startup.
//! Nothing in the codebase reads `std::env` directly — if a value is missing or
//! malformed the process refuses to boot instead of failing on the first request.

use std::env;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// Runtime configuration for the whole application.
#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,

    /// Public, client-reachable base URL (no trailing slash).
    pub registry_base_url: String,

    pub storage_dir: PathBuf,
    pub database_url: String,

    pub jwt_secret: String,
    pub session_ttl_minutes: i64,

    pub admin_username: String,
    pub admin_password: String,
    pub admin_email: String,

    pub allow_registration: bool,
    pub max_publish_body_bytes: usize,

    /// Upstream registries, tried in order. Empty disables proxying entirely.
    pub upstream_registries: Vec<String>,
    /// Per-request timeout for upstream calls.
    pub upstream_timeout_seconds: u64,

    pub cors_allowed_origins: Vec<String>,
    pub log_format: LogFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl Config {
    /// Load configuration from the process environment (after `.env`), validating
    /// every required field. Returns an error describing *all* problems at once.
    pub fn from_env() -> Result<Self> {
        let mut problems: Vec<String> = Vec::new();

        let host = opt("HOST").unwrap_or_else(|| "0.0.0.0".to_string());
        let port = parse_or("PORT", 4873, &mut problems);

        let registry_base_url = env::var("REGISTRY_BASE_URL")
            .unwrap_or_else(|_| format!("http://localhost:{port}"))
            .trim_end_matches('/')
            .to_string();
        if !(registry_base_url.starts_with("http://") || registry_base_url.starts_with("https://"))
        {
            problems.push("REGISTRY_BASE_URL must start with http:// or https://".into());
        }

        let storage_dir = PathBuf::from(opt("STORAGE_DIR").unwrap_or_else(|| "./storage".into()));
        let database_url =
            opt("DATABASE_URL").unwrap_or_else(|| "sqlite://./data/registry.db?mode=rwc".into());

        let jwt_secret = opt("JWT_SECRET").unwrap_or_default();
        if jwt_secret.len() < 32 {
            problems.push(
                "JWT_SECRET is required and must be at least 32 bytes (openssl rand -hex 32)"
                    .into(),
            );
        }

        let session_ttl_minutes = parse_or("SESSION_TTL_MINUTES", 720, &mut problems);
        if session_ttl_minutes <= 0 {
            problems.push("SESSION_TTL_MINUTES must be a positive integer".into());
        }

        let admin_username = opt("ADMIN_USERNAME").unwrap_or_default();
        let admin_password = opt("ADMIN_PASSWORD").unwrap_or_default();
        if admin_username.is_empty() && admin_password.is_empty() {
            // Silently allowed: an instance may pre-provision users via DB, but we
            // warn loudly in main() when no admin ends up existing.
        }
        let admin_email = opt("ADMIN_EMAIL").unwrap_or_else(|| "admin@example.com".into());

        let allow_registration = parse_or("ALLOW_REGISTRATION", false, &mut problems);

        let max_publish_body_mib: usize = parse_or("MAX_PUBLISH_BODY_MIB", 64, &mut problems);
        if max_publish_body_mib == 0 {
            problems.push("MAX_PUBLISH_BODY_MIB must be >= 1".into());
        }

        // Upstream chain. Order matters: the first upstream that answers wins,
        // and a 404 moves on to the next one. Taobao (npmmirror) is first by
        // default because it is markedly faster from mainland China.
        let upstream_registries = opt("UPSTREAM_REGISTRIES")
            .unwrap_or_else(|| {
                "https://registry.npmmirror.com,https://registry.npmjs.org".to_string()
            })
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        for registry in &upstream_registries {
            if !(registry.starts_with("http://") || registry.starts_with("https://")) {
                problems.push(format!(
                    "UPSTREAM_REGISTRIES entry '{registry}' must start with http:// or https://"
                ));
            }
        }

        let upstream_timeout_seconds: u64 = parse_or("UPSTREAM_TIMEOUT_SECONDS", 30, &mut problems);
        if upstream_timeout_seconds == 0 {
            problems.push("UPSTREAM_TIMEOUT_SECONDS must be >= 1".into());
        }

        let cors_allowed_origins = opt("CORS_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let log_format = match opt("LOG_FORMAT").unwrap_or_else(|| "text".into()).as_str() {
            "json" => LogFormat::Json,
            "text" | "" => LogFormat::Text,
            other => {
                problems.push(format!(
                    "LOG_FORMAT must be 'text' or 'json', got '{other}'"
                ));
                LogFormat::Text
            }
        };

        if !problems.is_empty() {
            bail!("invalid configuration:\n  - {}", problems.join("\n  - "));
        }

        Ok(Self {
            host,
            port,
            registry_base_url,
            storage_dir,
            database_url,
            jwt_secret,
            session_ttl_minutes,
            admin_username,
            admin_password,
            admin_email,
            allow_registration,
            max_publish_body_bytes: max_publish_body_mib * 1024 * 1024,
            upstream_registries,
            upstream_timeout_seconds,
            cors_allowed_origins,
            log_format,
        })
    }

    /// Absolute path of the SQLite file, when using the default file-backed URL.
    pub fn sqlite_file_path(&self) -> Option<PathBuf> {
        self.database_url
            .strip_prefix("sqlite://")
            .map(|rest| rest.split('?').next().unwrap_or(rest))
            .filter(|p| !p.is_empty() && *p != ":memory:")
            .map(PathBuf::from)
    }
}

fn opt(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_or<T>(key: &str, default: T, problems: &mut Vec<String>) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match opt(key) {
        None => default,
        Some(raw) => match raw.parse::<T>() {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!("{key} is invalid: {e}"));
                default
            }
        },
    }
}

/// Make sure the directories we are going to write into exist.
pub fn ensure_dirs(config: &Config) -> Result<()> {
    std::fs::create_dir_all(&config.storage_dir).with_context(|| {
        format!(
            "failed to create STORAGE_DIR '{}'",
            config.storage_dir.display()
        )
    })?;

    if let Some(db_path) = config.sqlite_file_path() {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create database directory '{}'", parent.display())
                })?;
            }
        }
    }
    Ok(())
}
