-- Upstream proxying is now a streaming relay: an upstream packument is passed
-- straight through to the client and never persisted. Drop the local upstream
-- cache an earlier revision created, along with the columns that supported it.
--
-- Rationale is in `src/upstream.rs`: parsing a full packument into
-- `serde_json::Value` costs roughly 10x the document size in resident memory
-- (a 15 MB `typescript` packument peaked near 150 MB), which is far too much
-- for a small host. Relaying keeps memory flat.

DELETE FROM packages WHERE source = 'upstream';

DROP INDEX IF EXISTS idx_packages_source;

ALTER TABLE packages DROP COLUMN source;
ALTER TABLE packages DROP COLUMN upstream_registry;
ALTER TABLE packages DROP COLUMN fetched_at;

ALTER TABLE versions DROP COLUMN upstream_tarball;
