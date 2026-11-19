use serde_json::json;

use super::*;

fn signed(mut files: PackageFiles) -> Vec<u8> {
    let signer = crate::package::tests::publisher("alice.dhttp.net", 'a');
    let digest = waz_payload(&files).unwrap();
    let modern = files.contains_key("manifest.toml");
    let mut input = if modern {
        b"pishoo-waz-signature-v2\0"
    } else {
        b"pishoo-waz-signature-v1\0"
    }
    .to_vec();
    for index in (0..digest.len()).step_by(2) {
        input.push(u8::from_str_radix(&digest[index..index + 2], 16).unwrap());
    }
    let signature = signer
        .sign(qtls::SignatureScheme::ECDSA_NISTP384_SHA384, &input)
        .unwrap();
    let certificates = signer
        .certificates()
        .iter()
        .map(|cert| STANDARD.encode(cert.as_ref()))
        .collect::<Vec<_>>();
    if modern {
        let envelope = json!({"signature":{"version":1,"certificates":certificates,"value":STANDARD.encode(signature)}});
        files
            .get_mut("manifest.toml")
            .unwrap()
            .extend_from_slice(toml::to_string_pretty(&envelope).unwrap().as_bytes());
    } else {
        files.insert("signature.json".into(), serde_json::to_vec(&json!({"version":1,"certificates":certificates,"signature":STANDARD.encode(signature)})).unwrap());
    }
    waz_write(&files).unwrap()
}

fn frontend() -> PackageFiles {
    let manifest = json!({"format":"waz/1","package":{"author":"alice.dhttp.net","name":"docs","version":"1.0.0",
            "title":"Docs","description":"Documentation","license":"MIT","release-notes":"First release"},
            "dependencies":[],"frontend":"public"});
    PackageFiles::from([
        (
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        ("public/index.html".into(), b"<h1>Docs</h1>".to_vec()),
    ])
}

#[test]
fn signed_packages_accept_only_optional_readme_and_changelog_documents() {
    for names in [
        vec![],
        vec!["README.md"],
        vec!["CHANGELOG.md"],
        vec!["README.md", "CHANGELOG.md"],
    ] {
        let mut files = frontend();
        for name in &names {
            files.insert((*name).into(), b"# Docs".to_vec());
        }
        let bytes = signed(files);
        let mut parsed = read_package(&bytes).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        for name in &names {
            assert_eq!(parsed[*name], b"# Docs");
        }
        if let Some(name) = names.first() {
            parsed.insert((*name).into(), b"# Changed docs".to_vec());
            assert!(read_package(&waz_write(&parsed).unwrap()).is_err());
        }
        parsed.remove("signature.json");
        assert!(read_package(&waz_write(&parsed).unwrap()).is_err());
    }
    let mut files = frontend();
    files.insert("metadata/openapi.json".into(), b"{}".to_vec());
    assert!(waz_write(&files).is_err());
}

#[test]
fn signed_lib_reads_embedded_openapi_directly_from_zip() {
    let document = json!({"openapi":"3.1.0","info":{"title":"Embedded API","version":"1.0.0",
            "description":"Embedded API","license":{"name":"MIT"}},
            "x-pishoo":{"publisher":"alice.dhttp.net","name":"embedded","dependencies":[]},"paths":{}});
    let bytes = crate::package::tests::package_fixture(
        &document,
        b"\0asm\x0d\0\x01\0",
        PackageFiles::new(),
        &crate::package::tests::publisher("alice.dhttp.net", 'a'),
    );
    let files = read_package(&bytes).unwrap();
    assert!(files.contains_key("backend.wasm"));
    assert_eq!(
        serde_json::from_slice::<Value>(package_openapi(&files).unwrap()).unwrap()["info"]["version"],
        "1.0.0"
    );
    assert!(package_metadata(&files).unwrap().get("host").is_none());
    assert_eq!(runtime_fixture(&bytes), files["backend.wasm"]);
}

#[test]
fn zip_validation_rejects_path_aliases_and_file_directory_conflicts() {
    for path in [
        "../outside",
        "/public/index.html",
        "public/../outside",
        "public//index.html",
        "public/%2e/index.html",
        "public/link\\file",
    ] {
        let mut files = frontend();
        files.insert(path.into(), b"bad".to_vec());
        assert!(waz_write(&files).is_err(), "{path}");
    }
    let mut files = frontend();
    files.insert("public/assets".into(), vec![]);
    files.insert("public/assets/app.js".into(), vec![]);
    assert!(waz_write(&files).is_err());
    for bytes in [&b"not a ZIP package"[..], &b"\0asm\x0d\0\x01\0"[..]] {
        assert!(read_package(bytes).is_err());
    }
}
#[test]
#[ignore = "requires explicitly selected real signed WAZ files via PISHOO_WAZ_TEST_FILES"]
fn real_signed_zip_packages_use_the_native_reader() {
    let paths = std::env::var_os("PISHOO_WAZ_TEST_FILES").expect("select signed package files");
    let mut count = 0;
    for path in std::env::split_paths(&paths) {
        let bytes = std::fs::read(&path).unwrap();
        let files =
            read_package(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let metadata = package_metadata(&files).unwrap();
        println!("verified {} @ {}", metadata["id"], metadata["version"]);
        assert!(files.contains_key("signature.json"));
        count += 1;
    }
    assert!(count > 0);
}

#[test]
fn exact_dependencies_omit_authors_and_reject_malformed_or_ambiguous_names() {
    for dependencies in [
        json!([{"name":"profile","version":"1.0.0"}]),
        json!([{"author":"alice.dhttp.net","name":"profile","version":"1.0.0"}]),
    ] {
        let mut files = frontend();
        let mut manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
        manifest["dependencies"] = dependencies.clone();
        files.insert(
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        );
        let bytes = signed(files);
        let mut parsed = read_package(&bytes).unwrap();
        let metadata = package_metadata(&parsed).unwrap();
        assert_eq!(metadata["dependencies"][0]["name"], "profile");
        assert_eq!(
            metadata["dependencies"][0].get("publisher").is_some(),
            dependencies[0].get("author").is_some()
        );
        manifest["dependencies"][0]["version"] = "2.0.0".into();
        parsed.insert(
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        );
        assert!(read_package(&waz_write(&parsed).unwrap()).is_err());
    }
    for dependencies in [
        json!([{"name":"profile","version":"1.0.0","path":"../profile"}]),
        json!([{"name":"profile","version":"01.0.0"}]),
        json!([{"name":"docs","version":"1.0.0"}]),
        json!([{"name":"profile","version":"1.0.0"},{"author":"bob.dhttp.net","name":"profile","version":"2.0.0"}]),
    ] {
        let mut files = frontend();
        let mut manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
        manifest["dependencies"] = dependencies;
        files.insert(
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        );
        assert!(read_package(&signed(files)).is_err());
    }
}

#[test]
fn waz_v2_toml_signature_boundary_and_payload_match_the_shared_vector() {
    let source = r#"format = "waz/2"
dependencies = []
frontend = "web"

[package]
author = "alice.dhttp.net"
name = "comm/demo"
version = "1.0.0"
title = "Demo"
description = "Signed documentation"
license = "MIT"
release-notes = "Initial"

[docs]
manual = "docs/manual.md"
readme = "https://example.com/readme"
"#;
    let mut files = std::collections::BTreeMap::from([
        ("manifest.toml".to_owned(), source.as_bytes().to_vec()),
        ("web/index.html".to_owned(), b"<h1>Demo</h1>".to_vec()),
        ("docs/manual.md".to_owned(), b"# Demo\n".to_vec()),
    ]);
    waz_layout(&files, &waz_manifest(&files).unwrap()).unwrap();
    assert_eq!(
        waz_payload(&files).unwrap(),
        "d59bc128cfbd4d8735c597f9c284d2a89365110287c6efb5d24ef6c594cad9e2"
    );
    let signature = b"[signature]\nversion = 1\ncertificates = ['certificate']\nvalue = 'first'\n";
    files
        .get_mut("manifest.toml")
        .unwrap()
        .extend_from_slice(signature);
    let signed_payload = waz_payload(&files).unwrap();
    assert_eq!(
        signed_payload,
        "d59bc128cfbd4d8735c597f9c284d2a89365110287c6efb5d24ef6c594cad9e2"
    );
    let mut changed = files.clone();
    changed.insert(
        "manifest.toml".into(),
        [
            source.as_bytes(),
            b"[signature]\nversion = 1\ncertificates = ['other']\nvalue = 'second'\n",
        ]
        .concat(),
    );
    assert_eq!(waz_payload(&changed).unwrap(), signed_payload);
    changed = files.clone();
    changed.insert("docs/manual.md".into(), b"# Changed\n".to_vec());
    assert_ne!(waz_payload(&changed).unwrap(), signed_payload);
    changed = files.clone();
    changed.insert(
        "manifest.toml".into(),
        [
            b"# Added comment\n".as_slice(),
            files["manifest.toml"].as_slice(),
        ]
        .concat(),
    );
    assert_ne!(waz_payload(&changed).unwrap(), signed_payload);
    assert_eq!(waz_signature(&files).unwrap()["signature"], "first");
    for suffix in ["[extra]\nvalue = 1\n", "[package]\nname = 'forged'\n"] {
        let mut bytes = files["manifest.toml"].clone();
        bytes.extend_from_slice(suffix.as_bytes());
        assert!(waz_toml(&bytes).is_err());
    }
    // Parser source spans distinguish a header-looking line inside a signed string.
    let multiline = source.replace(
        "description = \"Signed documentation\"",
        "description = '''\nLiteral text\n[signature]\ninside the description\n'''",
    );
    let multiline = [multiline.as_bytes(), signature].concat();
    let (document, boundary) = waz_toml(&multiline).unwrap();
    assert!(
        document["package"]["description"]
            .as_str()
            .unwrap()
            .contains("[signature]")
    );
    assert_eq!(&multiline[boundary..], signature);
    let dotted = format!("signature.value = 'forged'\n{source}");
    assert!(waz_toml(dotted.as_bytes()).is_err());
    let duplicated = format!("{source}format = 'waz/2'\nformat = 'waz/2'\n");
    assert!(waz_toml(duplicated.as_bytes()).is_err());
    files.insert("signature.json".into(), b"{}".to_vec());
    assert!(waz_layout(&files, &waz_manifest(&files).unwrap()).is_err());
}

#[test]
fn waz_v2_runtime_paths_document_links_and_format_are_checked() {
    let source = r#"format = "waz/2"
dependencies = []
frontend = "web"

[package]
author = "alice.dhttp.net"
name = "comm/demo"
version = "1.0.0"
title = "Demo"
description = "Signed documentation"
license = "MIT"
release-notes = "Initial"

[docs]
manual = "docs/manual.md"
readme = "https://example.com/readme"
"#;
    let mut files = std::collections::BTreeMap::from([
        ("manifest.toml".to_owned(), source.as_bytes().to_vec()),
        ("web/index.html".to_owned(), b"<h1>Demo</h1>".to_vec()),
        ("docs/manual.md".to_owned(), b"# Demo\n".to_vec()),
    ]);
    let document = waz_manifest(&files).unwrap();
    for (field, value) in [
        ("format", "waz/3"),
        ("wasm", "wasm/other.wasm"),
        ("web", "web"), // A legacy frontend declaration already exists.
        ("backend", "wasm/other.wasm"),
        ("frontend", "public"),
    ] {
        let mut changed = document.clone();
        changed[field] = value.into();
        assert!(waz_layout(&files, &changed).is_err());
    }
    for url in [
        "http://example.com/readme",
        "https://user:password@example.com/readme",
        "javascript:alert(1)",
    ] {
        let mut changed = document.clone();
        changed["docs"]["readme"] = url.into();
        assert!(waz_layout(&files, &changed).is_err());
    }
    let mut standard = document.clone();
    standard["package"]["name"] = "std/comm/demo".into();
    assert!(waz_layout(&files, &standard).is_err());
    standard["package"]["author"] = "dev.genmeta.tech.dhttp.net".into();
    waz_layout(&files, &standard).unwrap();
    files.remove("docs/manual.md");
    assert!(waz_layout(&files, &document).is_err());
    files.insert("docs/manual.md".into(), vec![0xff]);
    assert!(waz_layout(&files, &document).is_err());
}

#[test]
fn waz_v2_reader_verifies_embedded_signature_and_detects_manual_tampering() {
    let files = PackageFiles::from([
        (
            "manifest.toml".into(),
            r#"format = "waz/2"
dependencies = []
frontend = "web"

[package]
author = "alice.dhttp.net"
name = "comm/demo"
version = "1.0.0"
title = "Demo"
description = "Signed documentation"
license = "MIT"
release-notes = "Initial"

[docs]
manual = "docs/manual.md"
readme = "https://example.com/readme"
"#
            .as_bytes()
            .to_vec(),
        ),
        ("web/index.html".into(), b"<h1>Demo</h1>".to_vec()),
        ("docs/manual.md".into(), b"# Demo\n".to_vec()),
    ]);
    let bytes = signed(files);
    let mut files = read_package(&bytes).unwrap();
    assert_eq!(package_metadata(&files).unwrap()["name"], "comm/demo");
    assert!(files.contains_key("manifest.toml"));
    assert!(!files.contains_key("signature.json"));
    files.insert("docs/manual.md".into(), b"# Forged\n".to_vec());
    assert!(read_package(&waz_write(&files).unwrap()).is_err());
}
