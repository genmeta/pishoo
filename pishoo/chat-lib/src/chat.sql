CREATE TABLE IF NOT EXISTS module_versions (
    module_name TEXT PRIMARY KEY CHECK (length(module_name) > 0),
    version INTEGER NOT NULL CHECK (version >= 1)
);

CREATE TABLE IF NOT EXISTS chat_conversations (
    contact_name TEXT PRIMARY KEY CHECK (length(contact_name) > 0),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')
);

CREATE TABLE IF NOT EXISTS chat_capability_state (
    contact_name TEXT PRIMARY KEY CHECK (length(contact_name) > 0),
    subject_id BLOB CHECK (subject_id IS NULL OR length(subject_id) BETWEEN 1 AND 64),
    remote_message_granted INTEGER NOT NULL CHECK (remote_message_granted IN (0, 1)),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer')
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_name TEXT NOT NULL CHECK (length(contact_name) > 0),
    client_message_id TEXT NOT NULL CHECK (length(client_message_id) BETWEEN 1 AND 128),
    remote_message_id TEXT,
    direction TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
    state TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'received', 'failed', 'blocked')),
    sender_name TEXT NOT NULL CHECK (length(sender_name) > 0),
    recipient_name TEXT NOT NULL CHECK (length(recipient_name) > 0),
    text TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 4000),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_attempt_at INTEGER,
    next_attempt_at INTEGER,
    delivered_at INTEGER,
    created_at INTEGER NOT NULL CHECK (typeof(created_at) = 'integer'),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer'),
    error_message TEXT,
    UNIQUE (contact_name, client_message_id)
);

CREATE INDEX IF NOT EXISTS chat_messages_contact_created
    ON chat_messages (contact_name, id);

CREATE TABLE IF NOT EXISTS chat_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('send')),
    contact_name TEXT NOT NULL CHECK (length(contact_name) > 0),
    message_id INTEGER,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'completed', 'dead')),
    available_at INTEGER NOT NULL CHECK (typeof(available_at) = 'integer'),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    lease_token TEXT,
    lease_until INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL CHECK (typeof(created_at) = 'integer'),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer'),
    FOREIGN KEY (message_id) REFERENCES chat_messages(id)
);

CREATE UNIQUE INDEX IF NOT EXISTS chat_jobs_send_message
    ON chat_jobs (message_id) WHERE kind = 'send';

CREATE INDEX IF NOT EXISTS chat_jobs_due
    ON chat_jobs (state, available_at, lease_until);

CREATE TABLE capability_decisions (
    contact_name TEXT NOT NULL CHECK (length(contact_name) > 0),
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    decision TEXT NOT NULL CHECK (decision IN ('approved', 'denied', 'revoked')),
    descriptor_version TEXT NOT NULL CHECK (length(descriptor_version) > 0),
    subject_id BLOB NOT NULL CHECK (length(subject_id) BETWEEN 1 AND 64),
    request_id INTEGER NOT NULL CHECK (request_id > 0),
    updated_at INTEGER NOT NULL CHECK (typeof(updated_at) = 'integer'),
    PRIMARY KEY (contact_name, capability_id)
);

CREATE INDEX capability_decisions_updated
    ON capability_decisions (updated_at);

CREATE TABLE capability_decision_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_name TEXT NOT NULL CHECK (length(contact_name) > 0),
    capability_id TEXT NOT NULL CHECK (length(capability_id) > 0),
    decision TEXT NOT NULL CHECK (decision IN ('approved', 'denied', 'revoked')),
    descriptor_version TEXT NOT NULL CHECK (length(descriptor_version) > 0),
    subject_id BLOB NOT NULL CHECK (length(subject_id) BETWEEN 1 AND 64),
    request_id INTEGER NOT NULL CHECK (request_id > 0),
    decided_at INTEGER NOT NULL CHECK (typeof(decided_at) = 'integer')
);

CREATE INDEX capability_decision_events_contact
    ON capability_decision_events (contact_name, capability_id, id);
