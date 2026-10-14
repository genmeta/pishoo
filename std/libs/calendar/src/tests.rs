use super::*;

fn doc() -> Value {
    json!({"title":"项目评审", "description":"议程\n讨论", "location":"会议室", "start_at":1791939600000_i64,"end_at":1791943200000_i64,"all_day":false,"color":"blue"})
}

#[test]
fn validation_rejects_invalid_business_inputs() {
    for (key, value) in [
        ("title", json!(" ")),
        ("title", json!("a".repeat(121))),
        ("start_at", json!(1.5)),
        ("all_day", json!("true")),
        ("color", json!("red")),
        ("extra", json!(1)),
        ("end_at", json!(0)),
        ("location", json!("a\u{0000}b")),
    ] {
        let mut input = doc();
        input[key] = value;
        assert_eq!(validate(input, false).unwrap_err().0, 400, "{key}");
    }
    let mut input = doc();
    input["all_day"] = json!(true);
    assert!(validate(input.clone(), false).is_err());
    input["start_at"] = json!(1791936000000_i64);
    input["end_at"] = json!(1792022400000_i64);
    assert!(validate(input, false).is_ok());
    assert!(validate(doc(), true).is_err());
}

#[test]
fn persistence_overlap_and_optimistic_writes() {
    let mut db = Connection::open_in_memory().unwrap();
    initialize(&mut db).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let first = create(&db, id, &validate(doc(), false).unwrap(), 100).unwrap();
    assert_eq!(first["created_at"], 100);
    assert_eq!(
        events(&db, 1791940000000, 1791943200000).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        events(&db, 1791943200000, 1791944200000).unwrap()["events"],
        json!([])
    );
    assert_eq!(
        events(&db, 1791930000000, 1791939600000).unwrap()["events"],
        json!([])
    );
    let mut change = doc();
    change["updated_at"] = json!(100);
    change["title"] = json!("已更新");
    let updated = update(&mut db, id, &validate(change.clone(), true).unwrap(), 90).unwrap();
    assert_eq!(updated["updated_at"], 101);
    assert_eq!(updated["created_at"], 100);
    assert_eq!(update(&mut db, id, &change, 102).unwrap_err().0, 409);
    assert_eq!(delete(&mut db, id, 100).unwrap_err().0, 409);
    assert_eq!(event(&db, id).unwrap()["title"], "已更新");
    delete(&mut db, id, 101).unwrap();
    assert_eq!(event(&db, id).unwrap_err().0, 404);
    initialize(&mut db).unwrap();
}

#[test]
fn initialization_refuses_foreign_or_broken_databases() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE unrelated(id TEXT)").unwrap();
    assert!(initialize(&mut db).is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let mut db = Connection::open_in_memory().unwrap();
    db.pragma_update(None, "user_version", 2).unwrap();
    assert!(initialize(&mut db).is_err());
    let mut db = Connection::open_in_memory().unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    assert!(initialize(&mut db).is_err());
}

#[test]
fn duplicate_queries_and_invalid_ranges_are_rejected() {
    assert_eq!(query("/calendar/event?id=a&id=b").unwrap_err().0, 400);
    assert!(millis(Some("NaN")).is_err());
    assert!(millis(Some("-1")).is_err());
    assert!(event_id(&query("/calendar/event?id=../secret").unwrap()).is_err());
    let mut db = Connection::open_in_memory().unwrap();
    initialize(&mut db).unwrap();
    assert!(events(&db, 100, 100).is_err());
    assert!(events(&db, 0, 371 * 86400000).is_err());
}

#[test]
fn disk_persistence_and_concurrent_writes_keep_one_winner() {
    let file = std::env::temp_dir().join(format!(
        "pishoo-calendar-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    {
        let mut db = Connection::open(&file).unwrap();
        initialize(&mut db).unwrap();
        create(&db, id, &validate(doc(), false).unwrap(), 100).unwrap();
    }
    let outcomes = std::thread::scope(|scope| {
        let attempts = (0..2)
            .map(|i| {
                let file = &file;
                scope.spawn(move || {
                    let mut db = Connection::open(file).unwrap();
                    db.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
                    let mut change = doc();
                    change["updated_at"] = json!(100);
                    change["title"] = json!(format!("Writer {i}"));
                    update(&mut db, id, &validate(change, true).unwrap(), 101)
                })
            })
            .collect::<Vec<_>>();
        attempts
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err((409, _))))
            .count(),
        1
    );
    let db = Connection::open(&file).unwrap();
    assert_eq!(event(&db, id).unwrap()["updated_at"], 101);
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    std::fs::remove_file(file).unwrap();
}

#[test]
fn errors_use_standard_problem_details() {
    assert_eq!(
        problem(409, "Event version conflict."),
        json!({"type":"about:blank","title":"Conflict","status":409,"detail":"Event version conflict."})
    );
    assert_eq!(problem(400, "Invalid title.")["title"], "Bad Request");
    assert_eq!(problem(503, "Storage busy.")["status"], 503);
}
