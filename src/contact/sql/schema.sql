-- Contact database schema v6; used to initialize a new database.
CREATE TABLE outbound_contact_request (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_name TEXT NOT NULL CHECK (length(target_name) > 0),
    contact_id INTEGER,
    contact_subject TEXT,
    description TEXT NOT NULL DEFAULT '' CHECK (length(description) <= 80),
    requested_capabilities TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(requested_capabilities) AND json_type(requested_capabilities) = 'array'),
    offered_capabilities TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(offered_capabilities) AND json_type(offered_capabilities) = 'array'),
    requested_access TEXT CHECK (requested_access IS NULL OR (json_valid(requested_access) AND json_type(requested_access) = 'object')),
    offers TEXT CHECK (offers IS NULL OR (json_valid(offers) AND json_type(offers) = 'object')),
    application_id TEXT UNIQUE CHECK (application_id IS NULL OR length(application_id) = 64),
    status TEXT NOT NULL CHECK (status IN ('queued', 'pending', 'active', 'denied', 'expired', 'failed', 'revoked')),
    expired_after INTEGER NOT NULL CHECK (typeof(expired_after) = 'integer'),
    delivery_deadline INTEGER NOT NULL,
    remote_expired_after INTEGER,
    next_attempt_at INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    lease_until INTEGER,
    review_id INTEGER CHECK (review_id > 0),
    review_subject TEXT,
    review_method TEXT,
    review_path TEXT,
    last_checked_at INTEGER,
    error_message TEXT,
    created_at INTEGER NOT NULL CHECK (typeof(created_at) = 'integer'),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')
);

CREATE UNIQUE INDEX outbound_contact_request_pending_target
    ON outbound_contact_request (target_name) WHERE status IN ('queued', 'pending');

CREATE TABLE IF NOT EXISTS contact_entries (
    name TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('group','title')),
    alias TEXT,
    hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0,1)),
    nickname TEXT,
    avatar TEXT,
    host TEXT,
    host_owner_hash TEXT,
    member_joined_seq INTEGER,
    local_state TEXT CHECK (local_state IN ('active','leaving','left','removed','dissolved')),
    bound_name TEXT,
    bound_subject TEXT,
    source_service TEXT,
    source_subject TEXT,
    source_revision INTEGER NOT NULL CHECK (source_revision>0),
    created_at INTEGER NOT NULL,
    source_updated_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((kind='group' AND host IS NOT NULL AND host_owner_hash IS NOT NULL
        AND member_joined_seq>0 AND local_state IS NOT NULL AND nickname IS NOT NULL
        AND bound_name IS NULL AND bound_subject IS NULL)
      OR (kind='title' AND bound_name IS NOT NULL AND bound_subject IS NOT NULL
        AND host IS NULL AND host_owner_hash IS NULL AND member_joined_seq IS NULL AND local_state IS NULL))
);

CREATE TABLE IF NOT EXISTS directory_service_grants (
 service_id TEXT NOT NULL, author_owner_hash TEXT NOT NULL CHECK(length(author_owner_hash)=64),
 kind TEXT NOT NULL CHECK(kind IN ('name','group','title')),
 PRIMARY KEY(service_id,author_owner_hash,kind)
);
