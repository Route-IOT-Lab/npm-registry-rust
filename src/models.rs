//! Database row models. Deliberately thin — business rules live in `service`.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub is_admin: i64,
    pub is_active: i64,
    pub created_at: String,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.is_admin != 0
    }
    pub fn is_active(&self) -> bool {
        self.is_active != 0
    }
}

/// Public projection of a user — never leaks the password hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserView {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub created_at: String,
}

impl From<&User> for UserView {
    fn from(u: &User) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            email: u.email.clone(),
            is_admin: u.is_admin(),
            created_at: u.created_at.clone(),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Token {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    /// Only ever used for lookups and inserts — never serialised outward.
    #[allow(dead_code)]
    pub token_hash: String,
    pub token_prefix: String,
    pub source: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
}

/// Token listing projection — shows the prefix only, never the hash.
#[derive(Debug, Clone, Serialize)]
pub struct TokenView {
    pub id: i64,
    pub name: String,
    pub token: String,
    pub source: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
}

impl From<&Token> for TokenView {
    fn from(t: &Token) -> Self {
        Self {
            id: t.id,
            name: t.name.clone(),
            token: format!("{}…", t.token_prefix),
            source: t.source.clone(),
            created_at: t.created_at.clone(),
            last_used_at: t.last_used_at.clone(),
            expires_at: t.expires_at.clone(),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Package {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub keywords: Option<String>,
    pub owner_id: Option<i64>,
    pub downloads: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Version {
    pub id: i64,
    /// Selected via `SELECT *`; used for scoping in SQL, not read in Rust.
    #[allow(dead_code)]
    pub package_id: i64,
    pub version: String,
    pub manifest: String,
    pub tarball_name: String,
    pub tarball_path: String,
    pub shasum: String,
    pub integrity: String,
    pub size: i64,
    pub downloads: i64,
    #[allow(dead_code)]
    pub published_by: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct DistTag {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub package_id: i64,
    pub tag: String,
    pub version: String,
}
