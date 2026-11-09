//! Compile the actual Registry constants in both build modes; no host/network setup.
use std::{
    path::Path,
    process::{Command, Output},
};

const NAME: &str = "store.sandbox.dhttp.net";
const OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OFFICIAL: &str =
    "store.dhttp.net\nffc3e50e1dfb7baca1cc07a7cbfcf8cb34a9bf5a9c25024fcf9a9786bb230746\n";

fn compile(directory: &Path, debug: bool, name: Option<&str>, owner: Option<&str>) -> Output {
    let original = include_str!("../src/package/store.rs");
    let start = original.find("pub(crate) const STORE_NAME:").unwrap();
    let end = original.find("\nasync fn ").unwrap();
    let source = format!(
        "{}\nfn main() {{ println!(\"{{}}\\n{{}}\", STORE_NAME, STORE_OWNER); }}\n",
        &original[start..end]
    );
    let input = directory.join("pins.rs");
    std::fs::write(&input, source).unwrap();
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    command
        .arg("--edition=2024")
        .arg("-C")
        .arg(if debug {
            "debug-assertions=yes"
        } else {
            "debug-assertions=no"
        })
        .arg(&input)
        .arg("-o")
        .arg(directory.join("pins"))
        .env_remove("PISHOO_REGISTRY_NAME")
        .env_remove("PISHOO_REGISTRY_OWNER_HASH");
    if let Some(name) = name {
        command.env("PISHOO_REGISTRY_NAME", name);
    }
    if let Some(owner) = owner {
        command.env("PISHOO_REGISTRY_OWNER_HASH", owner);
    }
    command.output().unwrap()
}

fn execute(directory: &Path) -> String {
    let output = Command::new(directory.join("pins"))
        .env("PISHOO_REGISTRY_NAME", "runtime-change.dhttp.net")
        .env("PISHOO_REGISTRY_OWNER_HASH", "runtime-change")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn official_release_pins_ignore_runtime_environment() {
    let directory = tempfile::tempdir().unwrap();
    let output = compile(directory.path(), false, None, None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(execute(directory.path()), OFFICIAL);
}

#[test]
fn release_compilation_rejects_sandbox_overrides_including_empty_values() {
    for (name, owner) in [
        (Some(NAME), None),
        (None, Some(OWNER)),
        (Some(NAME), Some(OWNER)),
        (Some(""), None),
        (None, Some("")),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = compile(directory.path(), false, name, owner);
        assert!(
            !output.status.success(),
            "release accepted {name:?} / {owner:?}"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("only allowed in debug builds"));
    }
}

#[test]
fn debug_pins_require_compile_time_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let output = compile(directory.path(), true, Some(NAME), Some(OWNER));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(execute(directory.path()), format!("{NAME}\n{OWNER}\n"));
    let output = compile(directory.path(), true, None, None);
    assert!(output.status.success());
    assert_eq!(execute(directory.path()), OFFICIAL);
}
