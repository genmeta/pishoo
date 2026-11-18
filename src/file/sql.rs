//! Native file index queries and connection setup.
//! Callers own execution order and transaction boundaries; DDL stays in sql/.

pub(super) const CONNECTION: &str = r#"
PRAGMA temp_store=MEMORY;
PRAGMA mmap_size=0;
PRAGMA trusted_schema=OFF;
PRAGMA journal_mode=DELETE;
PRAGMA synchronous=FULL;
PRAGMA locking_mode=EXCLUSIVE;
"#;

pub(super) const LEGACY_HAS_STORAGE_NAME: &str = r#"
SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name='storage_name');
"#;

pub(super) const FILE_RECORD: &str = r#"
SELECT id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name
FROM native_files
WHERE id=?1;
"#;

pub(super) const CREATION_RESOURCE: &str = r#"
SELECT id
FROM native_files
WHERE request_id=?1;
"#;

pub(super) const CREATE_FILE: &str = r#"
INSERT INTO native_files(id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8,?9,?10);
"#;

pub(super) const COMMIT_OFFSET: &str = r#"
UPDATE native_files
SET offset=?1,checkpoint=?2,digest=?3
WHERE id=?4;
"#;

pub(super) const DELETE_FILE: &str = r#"
DELETE
FROM native_files
WHERE id=?1;
"#;

pub(super) const LEGACY_FILE_IDS: &str = "SELECT id FROM files;";

pub(super) const LEGACY_FILE_RECORD: &str = r#"
SELECT id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name
FROM files WHERE id=?1;
"#;

pub(super) const LEGACY_ID_FILE_RECORD: &str = r#"
SELECT id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,NULL
FROM files WHERE id=?1;
"#;

pub(super) const IMPORT_FILE: &str = r#"
INSERT INTO native_files(id,request_id,directory,name,media_type,size,offset,checkpoint,digest,created_at,storage_name)
VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
ON CONFLICT(id) DO NOTHING;
"#;
