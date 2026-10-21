-- These schemas share one rollback-journal transaction via setup::open_config.
CREATE TABLE IF NOT EXISTS lib_registry.lib (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT,
    installed_at INTEGER,
    upgraded_at INTEGER
);
CREATE TABLE IF NOT EXISTS app_registry.app (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT,
    installed_at INTEGER,
    upgraded_at INTEGER
);
CREATE TABLE IF NOT EXISTS app_registry.app_dependency (
    app_name TEXT NOT NULL REFERENCES app(name) ON DELETE CASCADE,
    lib_name TEXT NOT NULL,
    version TEXT NOT NULL,
    PRIMARY KEY(app_name, lib_name)
);
CREATE TABLE IF NOT EXISTS lib_registry.lib_dependency (
    lib_name TEXT NOT NULL REFERENCES lib(name) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL REFERENCES lib(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(lib_name, dependency_name)
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
