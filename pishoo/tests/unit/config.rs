use super::*;
#[test]
fn instance_config_rejects_unknown_fields_and_defaults_terminal_off() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("pishoo.toml");
    std::fs::write(&path, "state_dir='/tmp/example'\n").unwrap();
    assert!(!load_config(&path).unwrap().terminal.enabled);
    std::fs::write(&path, "state_dir='/tmp/example'\nrequest_limit=4\n").unwrap();
    assert!(load_config(&path).is_err());
    std::fs::write(
        &path,
        "state_dir='/tmp/example'\n[terminal]\nenabled=false\nbackend='native'\n",
    )
    .unwrap();
    assert!(load_config(&path).is_err());
}
