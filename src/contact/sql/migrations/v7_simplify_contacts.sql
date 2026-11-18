ALTER TABLE contact_entries RENAME TO contact_entries_v6;
-- Early v6 installations predate directory service grants; they have no grants to carry forward.
CREATE TABLE IF NOT EXISTS directory_service_grants (
    service_id TEXT NOT NULL, author_owner_hash TEXT NOT NULL, kind TEXT NOT NULL,
    PRIMARY KEY(service_id,author_owner_hash,kind)
);
ALTER TABLE directory_service_grants RENAME TO directory_service_grants_v6;

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

INSERT INTO contact_entries(name,bound_subject,alias,avatar,profile_updated_at,created_at,updated_at)
SELECT name,host_owner_hash,coalesce(nullif(alias,''),nullif(nickname,substr(name,instr(name,'~#')+2))),avatar,
       max(1,source_updated_at*1000),created_at,updated_at
FROM contact_entries_v6 WHERE kind='group';

INSERT INTO directory_service_grants (service_id, author_owner_hash, kind)
SELECT service_id, author_owner_hash, kind FROM directory_service_grants_v6 WHERE kind IN ('name','group');

DROP TABLE contact_entries_v6;
DROP TABLE directory_service_grants_v6;

ALTER TABLE outbound_contact_request RENAME TO outbound_v6;
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

WITH ranked AS (
    SELECT *, row_number() OVER (
        PARTITION BY target_name ORDER BY (status IN ('queued','pending')) DESC, id DESC
    ) AS priority
    FROM outbound_v6
), selected AS (
    SELECT *, (application_id IS NULL OR length(application_id)<>64 OR application_id GLOB '*[^a-f0-9]*'
        OR (contact_id IS NOT NULL AND contact_subject IS NULL)
        OR (requested_access IS NULL AND json_array_length(requested_capabilities)>0)
        OR (offers IS NULL AND json_array_length(offered_capabilities)>0)
        OR (review_id IS NOT NULL AND (review_subject IS NULL OR contact_subject IS NOT NULL AND contact_subject<>review_subject
            OR NOT (status='queued' AND review_method='POST' AND review_path='/std/contact'
                  OR status='pending' AND review_method='GET' AND review_path LIKE '/std/contact?name=%')))) AS invalid
    FROM ranked WHERE priority=1 AND status NOT IN ('active','revoked')
)
INSERT INTO outbound_contact_request(target_name,application_id,contact_id,target_subject,description,
    requested,offered,state,local_deadline,remote_deadline,review_id,next_attempt_at,last_error,created_at,updated_at)
SELECT target_name,CASE WHEN application_id IS NULL OR length(application_id)<>64 OR application_id GLOB '*[^a-f0-9]*'
                         THEN lower(hex(randomblob(32))) ELSE application_id END,
    CASE WHEN contact_subject IS NOT NULL THEN contact_id END,coalesce(contact_subject,review_subject),description,
    json_object('groups',json(requested_capabilities),'access',json(coalesce(requested_access,'{}'))),
    json_object('groups',json(offered_capabilities),'access',json(coalesce(offers,'{}'))),
    CASE WHEN invalid THEN 'failed' ELSE status END,delivery_deadline,remote_expired_after,
    CASE WHEN NOT invalid THEN review_id END,next_attempt_at,
    CASE WHEN invalid THEN 'legacy request cannot be verified; submit a new request' ELSE error_message END,created_at,updated_at
FROM selected;
DROP TABLE outbound_v6;
