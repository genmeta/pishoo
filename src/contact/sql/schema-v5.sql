-- Contact database schema v5; used to initialize a new database.
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
