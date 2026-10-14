use super::*;

fn component(id: &str, version: &str) -> Vec<u8> {
    let mut bytes =
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    fn leb(mut n: usize, output: &mut Vec<u8>) {
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            output.push(byte | if n == 0 { 0 } else { 128 });
            if n == 0 {
                break;
            }
        }
    }
    let document = serde_json::json!({
        "openapi": "3.1.0", "info": { "title": "test", "version": version },
        "paths": { format!("/{id}/upload"): { "post": { "responses": { "200": {"description": "ok"} } } } }
    })
    .to_string();
    let mut section = Vec::new();
    leb(14, &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(document.as_bytes());
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    package_component(&bytes)
}

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    for id in ["alpha", "beta"] {
        install_lib(&profile, id, &component(id, "1"), &runtime).unwrap();
    }
    profile
}

#[test]
fn successful_load_replaces_libs_and_removes_missing_versions() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let alpha = sandbox.libs["alpha"].clone();
    let task = sandbox.tasks.token();

    remove_lib(&profile, "beta").unwrap();
    install_lib(&profile, "alpha", &component("alpha", "2"), &runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let replacement = sandbox.libs["alpha"].clone();
    assert!(!Arc::ptr_eq(&alpha, &replacement));
    assert!(!sandbox.libs.contains_key("beta"));
    assert!(Arc::ptr_eq(&runtime, &sandbox.runtime));
    assert_eq!(sandbox.tasks.len(), 1);

    sandbox.close();
    sandbox.close();
    assert!(sandbox.libs.is_empty());
    assert!(sandbox.tasks.is_closed());
    assert_eq!(sandbox.tasks.len(), 1);
    drop(task);
    assert!(sandbox.tasks.is_empty());
}

#[test]
fn invalid_candidate_rejects_reload_and_retains_the_published_version() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.load_libs(&profile).unwrap();
    let original = sandbox.libs["alpha"].clone();
    std::fs::write(profile.join("lib/alpha/lib.webc"), b"invalid wasm").unwrap();
    assert!(matches!(
        sandbox.load_libs(&profile),
        Err(Error::InvalidComponent(_))
    ));
    assert!(Arc::ptr_eq(&original, &sandbox.libs["alpha"]));
}

#[test]
fn missing_registered_files_reject_reload_and_keep_loaded_libs() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.load_libs(&profile).unwrap();
    std::fs::remove_dir_all(profile.join("lib")).unwrap();
    assert!(sandbox.load_libs(&profile).is_err());
    assert_eq!(sandbox.libs.len(), 2);
}

#[test]
fn database_registration_controls_loading_and_file_versions_must_match() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime);
    std::fs::create_dir_all(profile.join("lib/extra")).unwrap();
    std::fs::write(profile.join("lib/extra/lib.webc"), component("extra", "1")).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute("DELETE FROM libs WHERE id='beta'", []).unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
    assert!(sandbox.libs.contains_key("alpha"));
    assert!(profile.join("lib/beta/lib.webc").is_file());
    std::fs::write(profile.join("lib/alpha/lib.webc"), component("alpha", "2")).unwrap();
    assert!(sandbox.load_libs(&profile).is_err());
    assert_eq!(sandbox.libs["alpha"].openapi.info.version, "1.0.0");
    assert_eq!(
        db.query_row("SELECT version FROM libs WHERE id='alpha'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "1.0.0"
    );
}
