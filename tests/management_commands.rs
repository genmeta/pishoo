//! The host rejects migrated commands without touching identity configuration.
use std::process::Command;

#[test]
fn host_keeps_only_run_reload_help_and_version() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["enable"],
        vec!["disable"],
        vec!["listen"],
        vec!["proxy"],
        vec!["lib", "list"],
        vec!["app", "check", "missing.waz"],
        vec!["start"],
        vec!["stop"],
        vec!["restart"],
        vec!["status"],
        vec!["reload", "--pid", "0"],
        vec!["reload", "--pid", "-1"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
            .env("HOME", home.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!home.path().join(".dhttp").exists());
    for arg in ["--help", "--version"] {
        assert!(
            Command::new(env!("CARGO_BIN_EXE_pishoo"))
                .arg(arg)
                .status()
                .unwrap()
                .success()
        );
    }
}
