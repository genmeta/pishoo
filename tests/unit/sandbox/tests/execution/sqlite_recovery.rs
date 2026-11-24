use super::*;

const PROBE: &[u8] = include_bytes!("../../../../fixtures/sqlite-recovery/probe.wasm");

fn setup(directory: &Path) -> Arc<Lib> {
    let db = rusqlite::Connection::open(directory.join("data.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE records(id INTEGER PRIMARY KEY, body);
         INSERT INTO records VALUES (1, 'committed');",
    )
    .unwrap();
    std::fs::write(directory.join("ordinary"), b"ordinary data").unwrap();
    load(PROBE, directory)
}

async fn assert_recovered(lib: Arc<Lib>, tasks: &TaskTracker, directory: &Path) {
    assert!(!directory.join("data.sqlite.lock").exists());
    // The interrupted invocation leaves the actual hot journal for SQLite.
    assert!(directory.join("data.sqlite-journal").is_file());
    let response = invoke(lib, tasks)
        .await
        .execute(request("/read", empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"1:committed");
    assert!(!directory.join("data.sqlite-journal").exists());
    assert!(!directory.join("data.sqlite.lock").exists());
    reaped(tasks).await;
}

#[tokio::test]
async fn next_request_recovers_sqlite_after_fuel_exhaustion() {
    let directory = tempfile::tempdir().unwrap();
    let lib = setup(directory.path());
    let tasks = TaskTracker::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        invoke(lib.clone(), &tasks)
            .await
            .execute(request("/early", empty())),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Guest(error)) if format!("{error:#}").contains("fuel")));
    assert!(directory.path().join("ordinary.lock").is_dir());
    assert_recovered(lib, &tasks, directory.path()).await;
}

#[tokio::test]
async fn cancelled_guest_releases_only_its_own_sqlite_lock() {
    let directory = tempfile::tempdir().unwrap();
    let lib = setup(directory.path());
    let tasks = TaskTracker::new();
    let response = invoke(lib.clone(), &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    assert!(directory.path().join("data.sqlite.lock").is_dir());

    let other = invoke(lib.clone(), &tasks)
        .await
        .execute(request("/read", empty()))
        .await
        .unwrap();
    assert_eq!(other.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        other
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .starts_with(b"database is locked")
    );
    assert!(directory.path().join("data.sqlite.lock").is_dir());

    drop(response);
    reaped(&tasks).await;
    assert_recovered(lib, &TaskTracker::new(), directory.path()).await;
}

#[tokio::test]
async fn cli_trap_allows_the_same_lib_to_serve_again() {
    let directory = tempfile::tempdir().unwrap();
    let lib = setup(directory.path());
    let tasks = TaskTracker::new();
    let result = Invocation::run(
        lib.clone(),
        crate::test_support::identity::endpoint("alice.dhttp.net"),
        tasks.clone(),
        Outgoing::Denied,
    )
    .await;
    assert!(matches!(result, Err(Error::Guest(_))));
    assert_recovered(lib, &tasks, directory.path()).await;
}
