CREATE TABLE IF NOT EXISTS libs (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT
);
CREATE TABLE IF NOT EXISTS apps (
    name TEXT PRIMARY KEY NOT NULL,
    author TEXT,
    version TEXT NOT NULL,
    title TEXT,
    description TEXT,
    license TEXT,
    repository TEXT
);
CREATE TABLE IF NOT EXISTS app_dependencies (
    app_name TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(app_name, lib_name)
);
CREATE TABLE IF NOT EXISTS lib_dependencies (
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL REFERENCES libs(name) ON DELETE RESTRICT,
    version TEXT NOT NULL,
    PRIMARY KEY(lib_name, dependency_name)
);

CREATE TABLE IF NOT EXISTS lib_capabilities (
    lib_name TEXT NOT NULL REFERENCES libs(name) ON DELETE CASCADE,
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (lib_name, capability_id)
);
CREATE TABLE IF NOT EXISTS lib_capability_operations (
    lib_name TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    method TEXT NOT NULL CHECK (length(method) > 0 AND method = upper(method)),
    path TEXT NOT NULL CHECK (substr(path, 1, 1) = '/' AND length(path) > 1),
    PRIMARY KEY (lib_name, capability_id, method, path),
    UNIQUE (method, path),
    FOREIGN KEY (lib_name, capability_id)
        REFERENCES lib_capabilities(lib_name, capability_id) ON DELETE CASCADE
);
