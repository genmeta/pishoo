CREATE TABLE administrator (
    name TEXT PRIMARY KEY,
    subject_id BLOB NOT NULL CHECK (length(subject_id) > 0)
);

CREATE TABLE device (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    first_administrator TEXT NOT NULL
);

-- The pending/deleting rows make filesystem publication recoverable after a crash.
CREATE TABLE managed_user (
    name TEXT PRIMARY KEY,
    subject_id BLOB NOT NULL,
    profile_path TEXT NOT NULL UNIQUE,
    phase TEXT NOT NULL CHECK (phase IN ('pending', 'active', 'deleting', 'removed'))
);

PRAGMA user_version = 2;
