-- Package registration schema v5: unified package registry and permission declarations.
-- Package metadata is the installation authority; content flags select runtime resources.
CREATE TABLE IF NOT EXISTS pkg_registry.package (
    name TEXT NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT,
    installed_at INTEGER,
    upgraded_at INTEGER,
    has_lib INTEGER NOT NULL CHECK(has_lib IN (0,1)),
    has_app INTEGER NOT NULL CHECK(has_app IN (0,1)),
    author_owner_hash TEXT,
    package_sha256 TEXT,
    signer_leaf_fingerprint_sha256 TEXT,
    PRIMARY KEY(name, version),
    CHECK(has_lib OR has_app)
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_active (
    name TEXT PRIMARY KEY NOT NULL,
    version TEXT NOT NULL,
    FOREIGN KEY(name, version) REFERENCES package(name, version) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE TRIGGER IF NOT EXISTS pkg_registry.initialize_package_active
AFTER INSERT ON package BEGIN
    INSERT INTO package_active(name,version) VALUES(NEW.name,NEW.version)
    ON CONFLICT(name) DO NOTHING;
END;
CREATE TABLE IF NOT EXISTS pkg_registry.package_root (
    name TEXT PRIMARY KEY NOT NULL,
    version TEXT NOT NULL,
    FOREIGN KEY(name, version) REFERENCES package(name, version) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_dependency (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    dependency_name TEXT NOT NULL,
    version TEXT NOT NULL,
    expected_author TEXT,
    CHECK(package_name <> dependency_name OR package_version <> version),
    PRIMARY KEY(package_name, package_version, dependency_name),
    FOREIGN KEY(package_name, package_version) REFERENCES package(name, version) ON UPDATE CASCADE ON DELETE CASCADE,
    FOREIGN KEY(dependency_name, version) REFERENCES package(name, version) ON UPDATE CASCADE ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_capability (
    lib_name TEXT NOT NULL,
    version TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    title TEXT NOT NULL,
    PRIMARY KEY(lib_name, version, capability_id),
    FOREIGN KEY(lib_name, version) REFERENCES package(name, version) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_operation (
    lib_name TEXT NOT NULL,
    version TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY(lib_name, version, capability_id, method, path),
    FOREIGN KEY(lib_name, version, capability_id)
        REFERENCES package_capability(lib_name, version, capability_id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_transaction (
    id TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE IF NOT EXISTS acl.lib_capability (
    lib_name TEXT NOT NULL,
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (lib_name, capability_id)
);
CREATE TABLE IF NOT EXISTS acl.lib_capability_operation (
    lib_name TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (length(method) > 0 AND method = upper(method)),
    path TEXT NOT NULL CHECK (substr(path, 1, 1) = '/' AND length(path) > 1),
    PRIMARY KEY (lib_name, capability_id, method, path),
    UNIQUE (method, path),
    FOREIGN KEY (lib_name, capability_id)
        REFERENCES lib_capability(lib_name, capability_id) ON DELETE CASCADE
);
