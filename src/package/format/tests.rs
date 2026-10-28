use serde_json::json;

use super::*;

fn signed(mut files: PackageFiles) -> Vec<u8> {
    let signer = crate::package::tests::publisher("alice.dhttp.net", 'a');
    let digest = waz_payload(&files);
    let mut input = b"pishoo-waz-signature-v1\0".to_vec();
    for index in (0..digest.len()).step_by(2) {
        input.push(u8::from_str_radix(&digest[index..index + 2], 16).unwrap());
    }
    let signature = signer
        .sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256, &input)
        .unwrap();
    files.insert("signature.json".into(), serde_json::to_vec(&json!({"version":1,
            "certificates":signer.certificates().iter().map(|cert|STANDARD.encode(cert.as_ref())).collect::<Vec<_>>(),
            "signature":STANDARD.encode(signature)})).unwrap());
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
