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
fn candidates_and_failed_verification_do_not_change_published_libs() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let runtime = Arc::new(Runtime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime.clone());
    let cancel = CancellationToken::new();
    let initial = sandbox.load_libs(&profile, &cancel).unwrap();
    assert!(sandbox.libs.is_empty());
    sandbox.verify_libs(&profile, &initial).unwrap();
    sandbox.replace_libs(initial);
    let alpha = sandbox.libs["alpha"].clone();
    let beta = sandbox.libs["beta"].clone();
    let task = sandbox.tasks.token();

    std::fs::remove_file(profile.join("lib/beta/lib.wasm")).unwrap();
    std::fs::write(profile.join("lib/alpha/lib.wasm"), component("2")).unwrap();
    let candidate = sandbox.load_libs(&profile, &cancel).unwrap();
    assert!(!candidate.contains_key("beta"));
    assert!(!Arc::ptr_eq(&alpha, &candidate["alpha"]));
    assert!(Arc::ptr_eq(&alpha, &sandbox.libs["alpha"]));
    assert!(!beta.cancel.is_cancelled());

    std::fs::write(profile.join("lib/alpha/lib.wasm"), component("3")).unwrap();
    assert!(matches!(
        sandbox.verify_libs(&profile, &candidate),
        Err(Error::InvalidComponent(_))
    ));
    assert!(Arc::ptr_eq(&alpha, &sandbox.libs["alpha"]));
    assert!(Arc::ptr_eq(&beta, &sandbox.libs["beta"]));
    assert!(!alpha.cancel.is_cancelled() && !beta.cancel.is_cancelled());

    std::fs::write(profile.join("lib/alpha/lib.wasm"), component("2")).unwrap();
    sandbox.verify_libs(&profile, &candidate).unwrap();
    sandbox.replace_libs(candidate);
    let replacement = sandbox.libs["alpha"].clone();
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
fn invalid_candidate_retains_the_published_version() {
    let directory = tempfile::tempdir().unwrap();
    let profile = profile(directory.path());
    let mut sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let cancel = CancellationToken::new();
    let initial = sandbox.load_libs(&profile, &cancel).unwrap();
    sandbox.replace_libs(initial);
    let original = sandbox.libs["alpha"].clone();
    std::fs::write(profile.join("lib/alpha/lib.wasm"), b"invalid wasm").unwrap();
    let candidate = sandbox.load_libs(&profile, &cancel).unwrap();
    sandbox.verify_libs(&profile, &candidate).unwrap();
    sandbox.replace_libs(candidate);
    assert!(Arc::ptr_eq(&original, &sandbox.libs["alpha"]));
    assert!(!original.cancel.is_cancelled());
}
