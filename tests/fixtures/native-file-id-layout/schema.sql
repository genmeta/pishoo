CREATE TABLE files (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    directory TEXT NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size >= 0),
    offset INTEGER NOT NULL CHECK(offset >= 0 AND offset <= size),
    checkpoint TEXT NOT NULL,
    digest TEXT,
    created_at INTEGER NOT NULL
);
