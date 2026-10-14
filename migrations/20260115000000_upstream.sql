-- Upstream proxying support.
--
-- Packages are either `local` (published to this registry) or `upstream`
-- (fetched from a configured upstream and cached here). Local always wins: a
-- name is resolved locally first and the upstream chain is only consulted on a
-- miss.

ALTER TABLE packages ADD COLUMN source TEXT NOT NULL DEFAULT 'local';

-- Which upstream served the cached copy, kept so we can explain provenance and
-- pick the same one on refresh.
ALTER TABLE packages ADD COLUMN upstream_registry TEXT;

-- When the cached metadata was last pulled. Drives the freshness check that
-- decides whether to re-fetch or serve straight from the database.
ALTER TABLE packages ADD COLUMN fetched_at TEXT;

-- Original tarball location on the upstream. Only needed when this registry
-- proxies and caches tarballs itself; when tarball caching is disabled the
-- packument keeps pointing straight at the upstream instead.
ALTER TABLE versions ADD COLUMN upstream_tarball TEXT;

CREATE INDEX IF NOT EXISTS idx_packages_source ON packages (source);
