ALTER TABLE administrator RENAME TO administrator_v1;

CREATE TABLE administrator (
    name TEXT PRIMARY KEY,
    subject_id BLOB NOT NULL CHECK (length(subject_id) > 0)
);

INSERT INTO administrator(name, subject_id)
SELECT name, subject_id FROM administrator_v1;

DROP TABLE administrator_v1;

PRAGMA user_version = 2;
