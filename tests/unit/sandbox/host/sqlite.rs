use super::*;

fn held_lock(path: &Path) -> (SqliteLocks, std::path::PathBuf) {
    let lock = path.join("database.lock");
    std::fs::create_dir(&lock).unwrap();
    let parent = File::open(path).unwrap();
    let metadata = parent.metadata().unwrap();
    let name = "database.lock".to_owned();
    let locks = BTreeMap::from([(
        (metadata.dev(), metadata.ino(), name.clone()),
        LockDirectory {
            parent,
            name,
            inode: File::open(&lock).unwrap(),
        },
    )]);
    let state = SqliteLocks {
        handles: Vec::new(),
        locks,
    };
    (state, lock)
}

#[tokio::test]
async fn abandoned_lock_waits_for_dispatched_storage_io() {
    let directory = tempfile::tempdir().unwrap();
    let (mut state, lock) = held_lock(directory.path());
    let handle = Arc::new(File::create(directory.path().join("database")).unwrap());
    state.track(&handle);
    drop(state);

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(lock.is_dir());
    drop(handle);
    tokio::time::timeout(Duration::from_secs(2), async {
        while lock.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn abandoned_lock_cleanup_preserves_a_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let (state, lock) = held_lock(directory.path());
    std::fs::remove_dir(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    drop(state);
    assert!(lock.is_dir());
}
