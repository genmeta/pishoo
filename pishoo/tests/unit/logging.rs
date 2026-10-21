use super::*;

#[test]
fn runtime_errors_and_warnings_append_to_the_global_error_log() {
    let root = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::for_user_home_dir(root.path());
    for index in 0..2 {
        tracing::subscriber::with_default(logging(&home).unwrap(), || {
            tracing::error!(identity = "alice", index, "test runtime error");
            tracing::warn!("test runtime warning");
        });
    }
    let path = home.join("log/error.log");
    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents.matches("test runtime error").count(), 2);
    assert_eq!(contents.matches("test runtime warning").count(), 2);
    assert!(contents.contains("identity=\"alice\""));
    assert!(!contents.contains('\u{1b}'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(home.join("log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn global_error_log_open_failure_is_returned_with_its_path() {
    let root = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::for_user_home_dir(root.path());
    let path = home.join("log/error.log");
    std::fs::create_dir_all(&path).unwrap();
    let error = match logging(&home) {
        Ok(_) => panic!("a directory cannot be opened as error.log"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(path.to_str().unwrap()));
}
