use super::*;

fn component(version: &str) -> Vec<u8> {
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
        "paths": { "/upload": { "post": { "responses": { "200": {"description": "ok"} } } } }
    })
    .to_string();
    let mut section = Vec::new();
    leb(14, &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(document.as_bytes());
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    bytes
}

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    for id in ["alpha", "beta"] {
        std::fs::create_dir_all(profile.join("lib").join(id)).unwrap();
        std::fs::write(
            profile.join("lib").join(id).join("lib.wasm"),
            component("1"),
        )
        .unwrap();
    }
    profile
}

#[test]
fn successful_load_replaces_libs_and_cancels_removed_versions() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let alpha = sandbox.libs["alpha"].clone();
    let beta = sandbox.libs["beta"].clone();
    let task = sandbox.tasks.token();

    std::fs::remove_dir_all(profile.join("lib/beta")).unwrap();
    std::fs::write(profile.join("lib/alpha/lib.wasm"), component("2")).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let replacement = sandbox.libs["alpha"].clone();
    assert!(!Arc::ptr_eq(&alpha, &replacement));
    assert!(beta.cancel.is_cancelled());
    assert!(!alpha.cancel.is_cancelled());
    assert!(Arc::ptr_eq(&runtime, &sandbox.runtime));
    assert_eq!(sandbox.tasks.len(), 1);

    sandbox.close();
    sandbox.close();
    assert!(sandbox.libs.is_empty());
    assert!(alpha.cancel.is_cancelled() && replacement.cancel.is_cancelled());
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
    std::fs::write(profile.join("lib/alpha/lib.wasm"), b"invalid wasm").unwrap();
    assert!(matches!(
        sandbox.load_libs(&profile),
        Err(Error::InvalidComponent(_))
    ));
    assert!(Arc::ptr_eq(&original, &sandbox.libs["alpha"]));
    assert!(!original.cancel.is_cancelled());
}

#[test]
fn removing_lib_root_cancels_all_loaded_libs() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.load_libs(&profile).unwrap();
    let alpha = sandbox.libs["alpha"].clone();
    std::fs::remove_dir_all(profile.join("lib")).unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.is_empty());
    assert!(alpha.cancel.is_cancelled());
}
