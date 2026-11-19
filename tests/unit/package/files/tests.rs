use serde_json::json;
use tower::ServiceExt;

use super::*;
use crate::{WasmRuntime, package::read_package};

#[tokio::test]
async fn access_groups_register_mounted_operations_and_populate_catalog() {
    for publisher in [
        "std.dhttp.net",
        "dev.genmeta.tech.dhttp.net",
        "alice.dhttp.net",
    ] {
        for legacy in [false, true] {
            for app in [false, true] {
                let title = if legacy {
                    json!("Receive")
                } else {
                    json!({"en":"Receive","zh-CN":"接收消息"})
                };
                let label = if legacy {
                    "Receive"
                } else {
                    "接收消息 / Receive"
                };
                let root = tempfile::tempdir().unwrap();
                let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
                crate::configure(&profile, None, true).unwrap();
                let mut db = crate::setup::open_config(
                    &profile,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                )
                .unwrap();
                let mut document = json!({"openapi":"3.1.0","info":{"title":"Message","version":"1.0.0","description":"test","license":{"name":"MIT"}},
                "x-pishoo":{"kind":"lib","publisher":publisher,"name":"message","dependencies":[]},
                "x-access-groups":{"message":{"title":title,"operations":["receive"],"app":app}},
                "paths":{"/comm/message":{"post":{"operationId":"receive","summary":"Receive messages","x-access-group":"message","responses":{"200":{"description":"ok"}}}}}});
                if legacy {
                    document["x-pishoo"]["access-groups"] = document
                        .as_object_mut()
                        .unwrap()
                        .remove("x-access-groups")
                        .unwrap();
                    document["paths"]["/comm/message"]["post"]["x-pishoo"] =
                        json!({"access-group":"message"});
                    document["paths"]["/comm/message"]["post"]
                        .as_object_mut()
                        .unwrap()
                        .remove("x-access-group");
                }
                let bytes = crate::package::tests::package_fixture(
                    &document,
                    include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
                    if app {
                        std::collections::BTreeMap::from([(
                            "index.html".parse().unwrap(),
                            b"hello".to_vec(),
                        )])
                    } else {
                        std::collections::BTreeMap::new()
                    },
                    &crate::package::tests::publisher(publisher, 'a'),
                );
                let package = read_package(&bytes).unwrap();
                let metadata = package_metadata(&package).unwrap();
                let tx = db.transaction().unwrap();
                save_package_identity(&tx, &format!("{publisher}/message"), &metadata).unwrap();
                save_package_relations(&tx, &format!("{publisher}/message"), &metadata, &package)
                    .unwrap();
                tx.commit().unwrap();
                let path = if crate::package::is_standard_author(publisher) {
                    "/std/comm/message"
                } else {
                    "/message/comm/message"
                };
                assert_eq!(
                    db.query_row(
                        "SELECT capability_id,title FROM lib_capability",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    )
                    .unwrap(),
                    (
                        "message".into(),
                        title
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| title.to_string())
                    )
                );
                assert_eq!(
                    db.query_row(
                        "SELECT method,path FROM lib_capability_operation WHERE method='POST'",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    )
                    .unwrap(),
                    ("POST".into(), path.into())
                );
                let response = crate::routes::capabilities_router(profile.clone())
                    .oneshot(
                        http::Request::builder()
                            .uri("/std/acl/permissions")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), http::StatusCode::OK);
                assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-store");
                let body = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let mut expected = json!({path:["POST"]});
                if app {
                    expected["/app/message"] = json!(["GET"]);
                }
                assert_eq!(
                    value,
                    json!({format!("{publisher}/message · {label}"):expected})
                );
                let response = crate::routes::capabilities_router(profile)
                    .oneshot(
                        http::Request::builder()
                            .uri("/std/acl/permissions?details=1")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), http::StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    json!({format!("{publisher}/message · {label}"):{"title":title,"endpoints":expected}})
                );
            }
        }
    }
}

#[test]
fn app_capability_rejects_missing_frontend_and_untrusted_targets() {
    for author in ["std.dhttp.net", "alice.dhttp.net"] {
        for (app, path, has_app) in [
            (json!(true), "/comm/message", false),
            (json!("/app/workspace"), "/comm/message", true),
            (json!({"path":"/std/acl"}), "/comm/message", true),
            (json!(true), "/app/workspace", true),
            (json!(true), "/app/message", true),
            (json!(true), "/app/message/../workspace", true),
        ] {
            let document = json!({"openapi":"3.1.0","info":{"title":"Test","version":"1.0.0","description":"test","license":{"name":"MIT"}},
                "x-pishoo":{"kind":"lib","publisher":author,"name":"message","dependencies":[]},
                "x-access-groups":{"read":{"title":"Read","operations":["read"],"app":app}},
                "paths":{path:{"get":{"operationId":"read","summary":"Read","x-access-group":"read","responses":{"200":{"description":"ok"}}}}}});
            let public = if has_app {
                std::collections::BTreeMap::from([(
                    "index.html".parse().unwrap(),
                    b"hello".to_vec(),
                )])
            } else {
                std::collections::BTreeMap::new()
            };
            let bytes = crate::package::tests::package_fixture(
                &document,
                include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
                public,
                &crate::package::tests::publisher(author, 'a'),
            );
            assert!(
                read_package(&bytes).is_err(),
                "accepted {author} {app} {path}"
            );
        }
    }
}

#[path = "tests/access_defaults.rs"]
mod access_defaults;

#[test]
fn registered_resources_remain_readable_under_preview_and_after_installation() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::configure(&profile, None, true).unwrap();
    let runtime = std::sync::Arc::new(WasmRuntime::new().unwrap());
    let package = |version: &str| {
        let document = json!({"openapi":"3.1.0","info":{"title":"Hello","version":version,"description":"test","license":{"name":"MIT"}},
            "x-pishoo":{"kind":"lib","publisher":"alice.dhttp.net","name":"hello","dependencies":[]},
            "paths":{"/read":{"post":{"operationId":"read","summary":"Read","responses":{"201":{"description":"Created"}}}}}});
        crate::package::tests::package_fixture(
            &document,
            include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
            std::collections::BTreeMap::from([(
                "index.html".parse().unwrap(),
                version.as_bytes().to_vec(),
            )]),
            &crate::package::tests::publisher("alice.dhttp.net", 'a'),
        )
    };
    let id = "alice.dhttp.net/hello";
    install_package(&profile, id, &package("1.0.0"), &runtime).unwrap();
    let lib = lib_root(&profile).unwrap();
    let original = disk_bytes(&profile, &lib, id).unwrap();
    let marker = profile.join("lib/hello.preview");
    std::fs::create_dir(&marker).unwrap();
    std::fs::rename(
        profile.join("lib/hello.wasm.1.0.0"),
        marker.join("previous-lib"),
    )
    .unwrap();
    std::fs::rename(profile.join("app/hello"), marker.join("previous-app")).unwrap();
    std::fs::write(profile.join("lib/hello.wasm.1.0.0"), b"unsigned preview").unwrap();
    std::fs::create_dir(profile.join("app/hello")).unwrap();
    std::fs::write(profile.join("app/hello/index.html"), b"preview page").unwrap();
    std::fs::write(
        marker.join("manifest.json"),
        serde_json::to_vec(&json!({
            "package":{"author":"alice.dhttp.net","name":"hello","version":"1.0.0"}
        }))
        .unwrap(),
    )
    .unwrap();
    let mut state = json!({});
    for (kind, path) in [("lib", "lib/hello.wasm.1.0.0"), ("app", "app/hello")] {
        let metadata = profile.join(path).metadata().unwrap();
        state[kind] = json!({"dev":metadata.dev(),"ino":metadata.ino()});
    }
    std::fs::write(
        marker.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert_eq!(disk_bytes(&profile, &lib, id).unwrap(), original);
    let mut sandbox = crate::sandbox::Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let apps = installed_apps(&profile, Some(id)).unwrap();
    let page = app_file(&apps[id].1, "index.html").unwrap();
    assert_eq!(read_runtime_wasm(page).unwrap(), b"1.0.0");
    install_package(&profile, id, &package("2.0.0"), &runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_ne!(disk_bytes(&profile, &lib, id).unwrap(), original);
    let apps = installed_apps(&profile, Some(id)).unwrap();
    assert_eq!(
        read_runtime_wasm(app_file(&apps[id].1, "index.html").unwrap()).unwrap(),
        b"2.0.0"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn waz_v2_installation_preserves_runtime_files_and_offline_manual() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};

    for (author, name) in [
        ("alice.dhttp.net", "comm/manual"),
        ("dev.genmeta.tech.dhttp.net", "std/comm/manual"),
    ] {
        let id = format!("{author}/{name}");
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
        crate::configure(&profile, None, true).unwrap();
        let signer = crate::package::tests::publisher(author, 'a');
        let document = json!({"openapi":"3.1.0","info":{"title":"Manual","version":"1.0.0","description":"Manual test","license":{"name":"MIT"}},
        "x-pishoo":{"publisher":author,"name":name,"dependencies":[]},
        "paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload","responses":{"200":{"description":"OK"}}}}}});
        let legacy = crate::package::tests::package_fixture(
            &document,
            include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
            std::collections::BTreeMap::from([("index.html".into(), b"manual app".to_vec())]),
            &signer,
        );
        let mut files = read_package(&legacy).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&files.remove("manifest.json").unwrap()).unwrap();
        files.remove("signature.json");
        manifest["format"] = "waz/2".into();
        let backend_path = format!("wasm/{name}.wasm");
        manifest["package"]["author"] = author.trim_end_matches(".dhttp.net").into();
        let fields = manifest.as_object_mut().unwrap();
        fields.remove("backend");
        fields.remove("frontend");
        manifest["wasm"] = backend_path.clone().into();
        manifest["web"] = "web".into();
        manifest["docs"] = json!({"manual":"docs/manual.md","readme":"https://example.com/manual"});
        let backend = files.remove("backend.wasm").unwrap();
        let frontend = files.remove("public/index.html").unwrap();
        files.insert(backend_path, backend.clone());
        files.insert("web/index.html".into(), frontend);
        files.insert(
            "docs/manual.md".into(),
            b"# comm/manual\n\n## NAME\n\nmanual - installation test\n".to_vec(),
        );
        files.insert(
            "manifest.toml".into(),
            toml::to_string_pretty(&manifest).unwrap().into_bytes(),
        );
        let payload = crate::package::format::test_payload_digest(&files);
        let mut input = b"pishoo-waz-signature-v2\0".to_vec();
        for offset in (0..payload.len()).step_by(2) {
            input.push(u8::from_str_radix(&payload[offset..offset + 2], 16).unwrap());
        }
        let signature = signer
            .sign(qtls::SignatureScheme::ECDSA_NISTP384_SHA384, &input)
            .unwrap();
        let envelope = json!({"signature":{"version":1,"certificates":signer.certificates().iter().map(|cert|STANDARD.encode(cert.as_ref())).collect::<Vec<_>>(),"value":STANDARD.encode(signature)}});
        files
            .get_mut("manifest.toml")
            .unwrap()
            .extend_from_slice(toml::to_string_pretty(&envelope).unwrap().as_bytes());
        let bytes = crate::package::format::waz_write(&files).unwrap();
        let runtime = std::sync::Arc::new(WasmRuntime::new().unwrap());
        install_package(&profile, &id, &bytes, &runtime).unwrap();
        assert_eq!(
            std::fs::read(profile.join(format!("lib/{name}.wasm.1.0.0"))).unwrap(),
            backend
        );
        assert_eq!(
            std::fs::read(profile.join(format!("app/{name}/index.html"))).unwrap(),
            b"manual app"
        );
        let details = installed_packages(&profile, Some(&id)).unwrap();
        assert_eq!(
            details["manual"],
            std::str::from_utf8(&files["docs/manual.md"]).unwrap()
        );
        assert_eq!(details["docs"]["readme"], "https://example.com/manual");
        let mut sandbox = crate::sandbox::Sandbox::new(runtime.clone());
        sandbox.load_libs(&profile).unwrap();
        assert!(profile.join(format!(".volume/{name}")).is_dir());
        if name.starts_with("std/") {
            assert!(!profile.join(".volume/std/std").exists());
            assert!(!profile.join("lib/std/std").exists());
        }
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(cached_release(&profile, &hash).unwrap().unwrap(), files);
        std::fs::write(
            profile.join(format!(".volume/sys/pkg/releases/{hash}.waz")),
            b"corrupt",
        )
        .unwrap();
        assert!(cached_release(&profile, &hash).is_err());
        assert!(cached_release(&profile, "../outside").is_err());
    }
}
