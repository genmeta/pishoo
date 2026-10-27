use std::process::Command;

#[test]
fn local_cli_initializes_current_schema_and_preserves_rejected_old_data() {
    let root = tempfile::Builder::new()
        .prefix("configuration space ")
        .tempdir()
        .unwrap();
    let profile = root.path().join("alice");
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_pishoo"))
            .args(["configure", "--profile"])
            .arg(&profile)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(!invoke(&[]).status.success());
    assert!(!invoke(&["--listen", "invalid"]).status.success());
    assert!(!profile.exists());
    for (args, expected) in [
        (vec!["--init"], "off"),
        (vec!["--listen", "all"], "all"),
        (vec!["--init"], "all"),
        (vec![], "all"),
        (vec!["--listen", "off"], "off"),
    ] {
        let result = invoke(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
            serde_json::json!({"listen":expected})
        );
    }
    let mut names = std::fs::read_dir(profile.join(".volume/sys"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["access", "contact", "pkg", "setting"]);
    let path = profile.join(".volume/sys/setting/database");
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    db.pragma_update(None, "user_version", 1).unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    for args in [vec![], vec!["--init"], vec!["--listen", "all"]] {
        let result = invoke(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("reinitialize"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    assert!(!profile.join(".volume/sys/lib").exists());
    assert!(!profile.join(".volume/sys/app").exists());
}
