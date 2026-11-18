-- Contact database schema v7; used to initialize a new database.
CREATE TABLE outbound_contact_request (
    target_name TEXT PRIMARY KEY NOT NULL CHECK (length(target_name)>0),
    application_id TEXT NOT NULL UNIQUE CHECK (length(application_id)=64 AND application_id NOT GLOB '*[^a-f0-9]*'),
    contact_id INTEGER,
    target_subject TEXT,
    description TEXT NOT NULL CHECK (length(description)<=80),
    requested TEXT NOT NULL CHECK (json_valid(requested) AND json_type(requested,'$.groups') IS 'array' AND json_type(requested,'$.access') IS 'object'),
    offered TEXT NOT NULL CHECK (json_valid(offered) AND json_type(offered,'$.groups') IS 'array' AND json_type(offered,'$.access') IS 'object'),
    state TEXT NOT NULL CHECK (state IN ('queued','pending','denied','expired','failed')),
    local_deadline INTEGER NOT NULL,
    remote_deadline INTEGER,
    review_id INTEGER CHECK (review_id>0),
    next_attempt_at INTEGER NOT NULL,
    lease_until INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (target_subject IS NOT NULL OR (contact_id IS NULL AND review_id IS NULL))
);

CREATE TABLE IF NOT EXISTS contact_entries (
    name TEXT PRIMARY KEY NOT NULL,
    bound_subject TEXT NOT NULL CHECK (length(bound_subject)=64 AND bound_subject NOT GLOB '*[^a-f0-9]*'),
    alias TEXT,
    avatar TEXT,
    profile_updated_at INTEGER NOT NULL CHECK (profile_updated_at>0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS directory_service_grants (
 service_id TEXT NOT NULL, author_owner_hash TEXT NOT NULL CHECK(length(author_owner_hash)=64),
 kind TEXT NOT NULL CHECK(kind IN ('name','group')),
 PRIMARY KEY(service_id,author_owner_hash,kind)
);
