CREATE TABLE IF NOT EXISTS libs (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT UNIQUE NOT NULL,
    version TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS apps (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT UNIQUE NOT NULL,
    version TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS app_dependencies (
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    lib_id TEXT NOT NULL REFERENCES libs(id) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(app_id, lib_id)
);
CREATE TABLE IF NOT EXISTS lib_dependencies (
    lib_id TEXT NOT NULL REFERENCES libs(id) ON DELETE CASCADE,
    dependency_id TEXT NOT NULL REFERENCES libs(id) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(lib_id, dependency_id)
);

CREATE TABLE IF NOT EXISTS lib_capabilities (
    lib_id TEXT NOT NULL REFERENCES libs(id) ON DELETE CASCADE,
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (lib_id, capability_id)
);
CREATE TABLE IF NOT EXISTS lib_capability_operations (
    lib_id TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (length(method) > 0 AND method = upper(method)),
    path TEXT NOT NULL CHECK (substr(path, 1, 5) = '/std/'),
    PRIMARY KEY (lib_id, capability_id, method, path),
    UNIQUE (method, path),
    FOREIGN KEY (lib_id, capability_id)
        REFERENCES lib_capabilities(lib_id, capability_id) ON DELETE CASCADE
);
