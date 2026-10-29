-- Package registry v3 -> v4. NULL means no developer-signing evidence.
-- Existing rows are never promoted to verified by migration.
ALTER TABLE pkg_registry.package ADD COLUMN author_owner_hash TEXT;
ALTER TABLE pkg_registry.package ADD COLUMN package_sha256 TEXT;
ALTER TABLE pkg_registry.package ADD COLUMN signer_leaf_fingerprint_sha256 TEXT;
PRAGMA pkg_registry.user_version=4;
