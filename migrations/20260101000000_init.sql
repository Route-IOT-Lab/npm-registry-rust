-- Initial schema for npm-registry-rust.
-- All timestamps are stored as RFC3339 UTC strings (SQLite has no native datetime).

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL COLLATE NOCASE,
    email         TEXT,
    password_hash TEXT    NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    is_active     INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (username)
);

CREATE TABLE IF NOT EXISTS tokens (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    -- SHA-256 hex of the opaque token. The plaintext is shown exactly once.
    token_hash   TEXT    NOT NULL,
    -- First characters of the token, safe to display in listings.
    token_prefix TEXT    NOT NULL,
    source       TEXT    NOT NULL DEFAULT 'web',
    created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    last_used_at TEXT,
    expires_at   TEXT,
    UNIQUE (token_hash)
);

CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens (user_id);

CREATE TABLE IF NOT EXISTS packages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL COLLATE NOCASE,
    description TEXT,
    readme      TEXT,
    homepage    TEXT,
    license     TEXT,
    keywords    TEXT,
    owner_id    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    downloads   INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (name)
);

CREATE TABLE IF NOT EXISTS versions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id   INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    version      TEXT    NOT NULL,
    -- The exact version manifest as submitted, minus the tarball attachment.
    manifest     TEXT    NOT NULL,
    tarball_name TEXT    NOT NULL,
    tarball_path TEXT    NOT NULL,
    shasum       TEXT    NOT NULL,
    integrity    TEXT    NOT NULL,
    size         INTEGER NOT NULL DEFAULT 0,
    downloads    INTEGER NOT NULL DEFAULT 0,
    published_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (package_id, version)
);

CREATE INDEX IF NOT EXISTS idx_versions_package ON versions (package_id);

CREATE TABLE IF NOT EXISTS dist_tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    tag        TEXT    NOT NULL,
    version    TEXT    NOT NULL,
    UNIQUE (package_id, tag)
);

CREATE TABLE IF NOT EXISTS maintainers (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE (package_id, user_id)
);

-- Short-lived handshake rows for the `npm login` web flow and CLI sessions.
CREATE TABLE IF NOT EXISTS login_sessions (
    id         TEXT    PRIMARY KEY,
    hostname   TEXT,
    user_id    INTEGER REFERENCES users(id) ON DELETE CASCADE,
    token      TEXT,
    status     TEXT    NOT NULL DEFAULT 'pending',
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    expires_at TEXT    NOT NULL
);
