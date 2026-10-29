CREATE TABLE outbound_contact_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_name TEXT NOT NULL CHECK (length(target_name) > 0),
    description TEXT NOT NULL DEFAULT '' CHECK (length(description) <= 80),
    requested_capabilities TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(requested_capabilities) AND json_type(requested_capabilities) = 'array'),
    offered_capabilities TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(offered_capabilities) AND json_type(offered_capabilities) = 'array'),
    application_id TEXT UNIQUE CHECK (application_id IS NULL OR length(application_id) = 64),
    status TEXT NOT NULL CHECK (status IN ('queued', 'pending', 'active', 'denied', 'expired', 'failed', 'revoked')),
    expired_after INTEGER NOT NULL CHECK (typeof(expired_after) = 'integer'),
    delivery_deadline INTEGER NOT NULL,
    remote_expired_after INTEGER,
    next_attempt_at INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    lease_until INTEGER,
    last_checked_at INTEGER,
    error_message TEXT,
    created_at INTEGER NOT NULL CHECK (typeof(created_at) = 'integer'),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')
);

CREATE UNIQUE INDEX outbound_contact_requests_pending_target
    ON outbound_contact_requests (target_name) WHERE status IN ('queued', 'pending');

CREATE TABLE saved_contacts (
    contact_name TEXT PRIMARY KEY CHECK (length(contact_name) > 0),
    contact_id INTEGER NOT NULL CHECK (contact_id > 0),
    subject_id BLOB NOT NULL CHECK (length(subject_id) BETWEEN 1 AND 64),
    saved_at INTEGER NOT NULL CHECK (typeof(saved_at) = 'integer')
);
