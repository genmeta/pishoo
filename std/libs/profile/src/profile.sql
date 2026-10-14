CREATE TABLE profile_preferences (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    display_name TEXT CHECK (display_name IS NULL OR length(display_name) BETWEEN 1 AND 80),
    avatar_name TEXT,
    gender TEXT,
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')
);

INSERT INTO profile_preferences (id, updated_at) VALUES (1, CAST(strftime('%s', 'now') AS INTEGER));
