use serde_json::json;
use tower::ServiceExt;

use super::*;
use crate::{WasmRuntime, package::read_package};

#[test]
fn release_cache_waits_for_journals_and_ignores_unjournaled_staging_directories() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::configure(&profile, None, true).unwrap();
    let parent = profile_dir(&profile).unwrap();
    let db = installation_database(&profile, false).unwrap();
    let stage = profile.join(".pishoo-0123456789abcdef0123456789abcdef");
    std::fs::create_dir(&stage).unwrap();
    let marker = stage.join(".transaction.json");
    std::fs::write(&marker, b"{}").unwrap();
    let archive = profile.join(".volume/sys/pkg/unused-1.0.0.waz");
    std::fs::write(&archive, b"unreferenced archive").unwrap();

    collect_release_cache(&parent, &db).unwrap();
    assert!(archive.is_file());

    std::fs::remove_file(marker).unwrap();
    collect_release_cache(&parent, &db).unwrap();
    assert!(!archive.exists());
    assert!(stage.is_dir());
}

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

#[tokio::test(flavor = "multi_thread")]
async fn waz_v2_installation_preserves_runtime_files_and_offline_manual() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};

    for (author, name) in [
        ("alice.dhttp.net", "comm/manual"),
        ("alice.dhttp.net", "database/manual"),
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
        let mount = name.strip_prefix("std/").unwrap_or(name);
        assert_eq!(
            std::fs::read(profile.join(format!("app/{mount}.1.0.0/index.html"))).unwrap(),
            b"manual app"
        );
        assert!(
            profile
                .join(format!("app/{mount}"))
                .symlink_metadata()
                .is_err()
        );
        if name != mount {
            assert!(!profile.join(format!("app/{name}")).exists());
            let old = profile.join(format!("app/{name}.1.0.0"));
            std::fs::create_dir_all(old.parent().unwrap()).unwrap();
            std::fs::rename(profile.join(format!("app/{mount}.1.0.0")), &old).unwrap();
            std::os::unix::fs::symlink("manual.1.0.0", profile.join(format!("app/{name}")))
                .unwrap();
            let mut sandbox = crate::sandbox::Sandbox::new(runtime.clone());
            sandbox.load_libs(&profile).unwrap();
            super::activate_installed_version(&profile, &id, "1.0.0", &mut sandbox)
                .unwrap()
                .unwrap();
            assert!(!old.exists());
            assert!(!profile.join(format!("app/{name}")).exists());
            assert!(!profile.join("app/std").exists());
            let router = crate::sandbox::tests::api_router(
                &sandbox,
                crate::test_support::identity::endpoint("alice"),
            );
            for path in [format!("/app/{mount}/"), format!("/app/{name}/")] {
                let response = router
                    .clone()
                    .oneshot(
                        http::Request::builder()
                            .uri(&path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), http::StatusCode::OK, "{path}");
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap(),
                    "manual app"
                );
            }

            assert!(
                profile
                    .join(format!("app/{mount}"))
                    .symlink_metadata()
                    .is_err()
            );
        }
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
        let cache = profile.join(".volume/sys/pkg");
        let named = cache.join(format!(
            "{}-1.0.0.waz",
            name.replacen("database/", "database~/", 1)
        ));
        let legacy_dir = cache.join("releases");
        let legacy = legacy_dir.join(format!("{hash}.waz"));
        std::fs::create_dir(&legacy_dir).unwrap();
        assert_eq!(
            cached_release(&profile, &details, &hash).unwrap().unwrap(),
            files
        );
        std::fs::rename(&named, &legacy).unwrap();
        assert_eq!(
            cached_release(&profile, &details, &hash).unwrap().unwrap(),
            files
        );
        let db = installation_database(&profile, false).unwrap();
        collect_release_cache(&profile_dir(&profile).unwrap(), &db).unwrap();
        assert!(!legacy_dir.exists());
        assert_eq!(std::fs::read(&named).unwrap(), bytes);
        let legacy_named = legacy_dir.join(format!("{name}-1.0.0.waz"));
        std::fs::create_dir_all(legacy_named.parent().unwrap()).unwrap();
        std::fs::rename(&named, &legacy_named).unwrap();
        assert_eq!(
            cached_release(&profile, &details, &hash).unwrap().unwrap(),
            files
        );
        collect_release_cache(&profile_dir(&profile).unwrap(), &db).unwrap();
        assert!(!legacy_dir.exists());
        assert_eq!(std::fs::read(&named).unwrap(), bytes);
        std::fs::write(named, b"corrupt").unwrap();
        assert!(cached_release(&profile, &details, &hash).is_err());
        assert!(cached_release(&profile, &details, "../outside").is_err());
    }
}
