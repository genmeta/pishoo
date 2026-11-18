use std::os::unix::fs::MetadataExt;

use http::StatusCode;
use tower::ServiceExt;

use super::*;

fn library(sandbox: &Sandbox, directory: &Path, id: &str, version: &str) -> Arc<Lib> {
    let mut lib = super::api::load(sandbox, directory, r#"{"/comm/message":{"post":{}}}"#);
    let loaded = Arc::get_mut(&mut lib).unwrap();
    loaded.id = id.to_owned();
    loaded.openapi.info.version = version.to_owned();
    lib
}

fn request(version: Option<&str>) -> Request<axum::body::Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("https://alice.dhttp.net/std/comm/message");
    if let Some(version) = version {
        builder = builder.header("pishoo-package-version", version);
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

#[tokio::test]
async fn exact_http_versions_and_trusted_dependency_context_select_distinct_libraries() {
    let directory = tempfile::tempdir().unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let id = "dev.genmeta.tech.dhttp.net/std/comm/message";
    let old = library(&sandbox, directory.path(), id, "1.0.0");
    let new = library(&sandbox, directory.path(), id, "2.0.0");
    sandbox
        .versions
        .insert((id.to_owned(), "1.0.0".to_owned()), old.clone());
    sandbox
        .versions
        .insert((id.to_owned(), "2.0.0".to_owned()), new.clone());
    sandbox.libs.insert(id.to_owned(), new.clone());
    let endpoint = crate::test_support::identity::endpoint("alice");
    assert!(Arc::ptr_eq(
        &select_library(&sandbox, &endpoint, &request(None))
            .unwrap()
            .0,
        &new
    ));
    assert!(Arc::ptr_eq(
        &select_library(&sandbox, &endpoint, &request(Some("1.0.0")))
            .unwrap()
            .0,
        &old
    ));
    assert!(matches!(
        select_library(&sandbox, &endpoint, &request(Some("3.0.0"))),
        Err(Error::PackagePolicy {
            code: "package_version_unavailable",
            status: StatusCode::NOT_ACCEPTABLE
        })
    ));
    assert!(matches!(
        select_library(&sandbox, &endpoint, &request(Some("1.0"))),
        Err(Error::PackagePolicy {
            code: "invalid_package_version",
            ..
        })
    ));
    let mut caller = library(&sandbox, directory.path(), "alice.dhttp.net/a", "1.0.0");
    Arc::get_mut(&mut caller)
        .unwrap()
        .dependencies
        .insert("std/comm/message".to_owned(), old.clone());
    let mut pinned = request(None);
    pinned.extensions_mut().insert(caller.clone());
    assert!(Arc::ptr_eq(
        &select_library(&sandbox, &endpoint, &pinned).unwrap().0,
        &old
    ));
    let mut conflicting = request(Some("2.0.0"));
    conflicting.extensions_mut().insert(caller);
    assert!(matches!(
        select_library(&sandbox, &endpoint, &conflicting),
        Err(Error::PackagePolicy {
            code: "dependency_version_conflict",
            ..
        })
    ));
    let router = super::api_router(&sandbox, endpoint.clone());
    let mut exact = request(Some("1.0.0"));
    exact.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(endpoint.local_authority().unwrap()),
        remote: None,
    });
    let response = router.oneshot(exact).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("pishoo-package-version")
            .and_then(|value| value.to_str().ok()),
        Some("1.0.0"),
        "unexpected response status {}",
        response.status()
    );
    assert!(
        response
            .headers()
            .get_all(http::header::VARY)
            .iter()
            .any(|value| value.to_str().unwrap().contains("Pishoo-Package-Version"))
    );
}

#[test]
fn versions_preopen_the_same_business_volume() {
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host;
    fn volume(lib: &Lib) -> std::fs::File {
        let mut filesystem = lib.filesystem.clone();
        let mut table = ResourceTable::new();
        let directories = wasmtime_wasi::filesystem::WasiFilesystemCtxView {
            ctx: &mut filesystem,
            table: &mut table,
        }
        .get_directories()
        .unwrap();
        let (descriptor, _) = directories
            .into_iter()
            .find(|(_, path)| path == "/db")
            .unwrap();
        let wasmtime_wasi::filesystem::Descriptor::Dir(directory) = table.get(&descriptor).unwrap()
        else {
            panic!("expected directory");
        };
        directory.dir.try_clone().unwrap()
    }
    let directory = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let old = library(
        &sandbox,
        directory.path(),
        "dev.genmeta.tech.dhttp.net/std/comm/message",
        "1.0.0",
    );
    let new = library(
        &sandbox,
        directory.path(),
        "dev.genmeta.tech.dhttp.net/std/comm/message",
        "2.0.0",
    );
    let old = volume(&old);
    let new = volume(&new);
    assert_eq!(old.metadata().unwrap().dev(), new.metadata().unwrap().dev());
    assert_eq!(old.metadata().unwrap().ino(), new.metadata().unwrap().ino());
    let old = cap_std::fs::Dir::from_std_file(old);
    let new = cap_std::fs::Dir::from_std_file(new);
    old.write("database", b"shared data").unwrap();
    assert_eq!(new.read("database").unwrap(), b"shared data");
}
