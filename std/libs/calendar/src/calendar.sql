CREATE TABLE events (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 120),
    description TEXT NOT NULL CHECK(length(description) <= 8000),
    location TEXT NOT NULL CHECK(length(location) <= 240),
    start_at INTEGER NOT NULL CHECK(start_at >= 0 AND start_at < 253402300800000),
    end_at INTEGER NOT NULL CHECK(end_at > start_at AND end_at < 253402300800000),
    all_day INTEGER NOT NULL CHECK(all_day IN (0, 1)),
    color TEXT NOT NULL CHECK(color IN ('blue', 'green', 'yellow', 'purple', 'pink')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK(all_day = 0 OR (start_at % 86400000 = 0 AND end_at % 86400000 = 0))
);
CREATE INDEX events_time ON events(start_at, end_at);
