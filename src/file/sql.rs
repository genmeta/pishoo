//! Native file index queries and connection setup.
//! Callers own execution order and transaction boundaries; DDL stays in sql/.

pub(super) const CONNECTION: &str = r#"
PRAGMA temp_store=MEMORY;
PRAGMA mmap_size=0;
PRAGMA trusted_schema=OFF;
PRAGMA journal_mode=DELETE;
PRAGMA synchronous=FULL;
"#;

pub(super) const HAS_STORAGE_NAME: &str = r#"
SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name='storage_name');
"#;

pub(super) const FILE_RECORD: &str = r#"
SELECT id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name
FROM files
WHERE id=?1;
"#;

pub(super) const CREATION_RESOURCE: &str = r#"
SELECT id
FROM files
WHERE request_id=?1;
"#;

pub(super) const CREATE_FILE: &str = r#"
INSERT INTO files(id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8,?9,?10);
"#;

pub(super) const COMMIT_OFFSET: &str = r#"
UPDATE files
SET offset=?1,checkpoint=?2,digest=?3
WHERE id=?4;
"#;

pub(super) const DELETE_FILE: &str = r#"
DELETE
FROM files
WHERE id=?1;
"#;
