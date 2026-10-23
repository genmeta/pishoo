-- Package registration schema v3: unified package registry and permission declarations.
-- Package metadata is the installation authority; content flags select runtime resources.
CREATE TABLE IF NOT EXISTS pkg_registry.package (
    name TEXT PRIMARY KEY NOT NULL,
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
    CHECK(has_lib OR has_app)
);
CREATE TABLE IF NOT EXISTS pkg_registry.package_dependency (
    package_name TEXT NOT NULL REFERENCES package(name) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL REFERENCES package(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    CHECK(package_name <> dependency_name),
    PRIMARY KEY(package_name, dependency_name)
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
