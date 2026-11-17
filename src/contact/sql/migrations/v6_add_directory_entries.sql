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
