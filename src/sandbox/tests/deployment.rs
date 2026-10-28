use serde_json::json;

use super::*;
use crate::package::tests::{
    package_fixture,
    packages::{frontend_document, frontend_files},
    publisher,
};

fn component(id: &str, version: &str) -> Vec<u8> {
    let mut bytes =
        include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
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
fn startup_reads_raw_components_while_inputs_require_source_signatures() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let root = lib_root(&profile).unwrap();
    let bytes = disk_bytes(&profile, &root, "alpha").unwrap();
    assert!(read_package(&bytes).is_err());
    assert!(wasmparser::Parser::is_component(&bytes));
    assert_eq!(
        bytes,
        std::fs::read(profile.join("lib/alpha.wasm")).unwrap()
    );
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    assert!(install_lib(&profile, "alpha", &bytes, &runtime).is_err());
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 2);
    assert_eq!(sandbox.libs["alpha"].openapi.info.version, "1.0.0");
    assert!(
        installed_libs(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("error").is_none())
    );
}

#[test]
fn invalid_candidate_rejects_reload_and_retains_the_published_version() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.load_libs(&profile).unwrap();
    let original = sandbox.libs["alpha"].clone();
    std::fs::write(profile.join("lib/alpha.wasm"), b"invalid wasm").unwrap();
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
fn database_registration_controls_loading_without_revalidating_versions() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime);
    std::fs::create_dir_all(profile.join("lib/extra")).unwrap();
    std::fs::write(
        profile.join("lib/extra/.unused-payload"),
        component("extra", "1"),
    )
    .unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("DELETE FROM package WHERE name='beta'", [])
        .unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
    assert!(sandbox.libs.contains_key("alpha"));
    assert!(profile.join("lib/beta.wasm").is_file());
    db.execute("UPDATE package SET version='2.0.0' WHERE name='alpha'", [])
        .unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs["alpha"].openapi.info.version, "1.0.0");
    assert_eq!(
        db.query_row("SELECT version FROM lib WHERE name='alpha'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "2.0.0"
    );
}
#[test]
fn startup_uses_current_runtime_files_without_integrity_comparison_or_repair() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut document = frontend_document("1.0.0");
    document["x-pishoo"]["kind"] = "lib".into();
    document["x-pishoo"]
        .as_object_mut()
        .unwrap()
        .remove("entry");
    document["paths"] = json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
    let id = "alice.dhttp.net/hello";
    let source = package_fixture(
        &document,
        include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm"),
        std::collections::BTreeMap::new(),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_lib(&profile, id, &source, &runtime).unwrap();
    crate::package::install_package(
        &profile,
        id,
        &package_fixture(
            &document,
            include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm"),
            frontend_files("original"),
            &publisher("alice.dhttp.net", 'a'),
        ),
        &runtime,
    )
    .unwrap();
    let wasm = profile.join("lib/hello.wasm");
    let mut changed = std::fs::read(&wasm).unwrap();
    let name = b"local-note";
    let mut payload = vec![name.len() as u8];
    payload.extend_from_slice(name);
    payload.extend_from_slice(b"edited after installation");
    changed.push(0);
    let mut size = payload.len();
    while size >= 0x80 {
        changed.push((size as u8 & 0x7f) | 0x80);
        size >>= 7;
    }
    changed.push(size as u8);
    changed.extend_from_slice(&payload);
    std::fs::write(&wasm, &changed).unwrap();
    let page = profile.join("app/hello/index.html");
    std::fs::write(&page, b"locally edited page").unwrap();
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(std::fs::read(&wasm).unwrap(), changed);
    assert_eq!(
        disk_bytes(&profile, &lib_root(&profile).unwrap(), id).unwrap(),
        changed
    );
    assert_eq!(std::fs::read(&page).unwrap(), b"locally edited page");
    assert_eq!(
        std::io::read_to_string(app_file(&sandbox.apps[id].1, "index.html").unwrap()).unwrap(),
        "locally edited page"
    );
    assert!(profile.join(".volume/hello").is_dir());
    assert!(!profile.join("db/alice.dhttp.net").exists());
    assert!(!profile.join("lib/alice.dhttp.net").exists());
    assert!(!profile.join("app/alice.dhttp.net").exists());
}

#[test]
fn raw_component_loading_rejects_missing_duplicate_and_oversized_metadata() {
    fn append(bytes: &mut Vec<u8>, data: &[u8]) {
        let name = b"pishoo:openapi";
        let mut section = vec![name.len() as u8];
        section.extend_from_slice(name);
        section.extend_from_slice(data);
        bytes.push(0);
        let mut length = section.len();
        while length >= 128 {
            bytes.push((length as u8 & 127) | 128);
            length >>= 7;
        }
        bytes.push(length as u8);
        bytes.extend(section);
    }
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let original =
        include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm");
    assert!(
        Lib::load(
            runtime.clone(),
            "raw".into(),
            original,
            &root.path().join("missing")
        )
        .is_err()
    );
    let document =
        br#"{"openapi":"3.1.0","info":{"title":"Raw component","version":"1.0.0"},"paths":{}}"#;
    let mut bytes = original.to_vec();
    append(&mut bytes, document);
    assert_eq!(
        Lib::load(
            runtime.clone(),
            "raw".into(),
            &bytes,
            &root.path().join("valid")
        )
        .unwrap()
        .openapi
        .info
        .version,
        "1.0.0"
    );
    append(&mut bytes, document);
    assert!(
        Lib::load(
            runtime.clone(),
            "raw".into(),
            &bytes,
            &root.path().join("duplicate")
        )
        .is_err()
    );
    let mut bytes = original.to_vec();
    append(&mut bytes, &vec![b' '; 1024 * 1024 + 1]);
    assert!(
        Lib::load(
            runtime,
            "raw".into(),
            &bytes,
            &root.path().join("oversized")
        )
        .is_err()
    );
    assert!(!root.path().join("duplicate").exists());
    assert!(!root.path().join("oversized").exists());
}
