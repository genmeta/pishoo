//! Real pack/check executables with generated credentials in isolated homes.
use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn identity(home: &Path) {
    let name = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("1:0:{}", "a".repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let ssl = home.join("alice/ssl");
    std::fs::create_dir_all(&ssl).unwrap();
    std::fs::write(ssl.join("fullchain.crt"), cert.pem()).unwrap();
    std::fs::write(ssl.join("privkey.pem"), key.serialize_pem()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            ssl.join("privkey.pem"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
    }
    std::fs::write(ssl.join("ocsp.der"), [1]).unwrap();
}

fn project(path: &Path, kind: &str, repository: &str) {
    std::fs::create_dir_all(path).unwrap();
    let content = if kind == "app" {
        "entry='/'"
    } else {
        "host={}"
    };
    let mapping = if kind == "app" {
        "'/public'='public'"
    } else {
        "'/metadata'='metadata'"
    };
    let module = if kind == "app" {
        ""
    } else {
        "[[module]]\nname='backend'\nsource='lib.wasm'\n"
    };
    std::fs::write(path.join("wasmer.toml"), format!(
        "[package]\nname='alice.dhttp.net/hello'\nversion='1.0.0'\ndescription='Test package'\nlicense='MIT'\n{repository}\n[package.pishoo]\nkind='{kind}'\ntitle='Hello'\nchangelog='Initial release'\ndependencies=[]\n{content}\n[fs]\n{mapping}\n{module}"
    )).unwrap();
    if kind == "app" {
        std::fs::create_dir(path.join("public")).unwrap();
        std::fs::write(path.join("public/index.html"), "<h1>Hello</h1>").unwrap();
    } else {
        std::fs::create_dir(path.join("metadata")).unwrap();
        std::fs::write(path.join("metadata/openapi.json"), serde_json::to_vec(&json!({
            "openapi":"3.1.0","info":{"title":"Hello","version":"1.0.0"},
            "paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload","responses":{"200":{"description":"OK"}}}}}
        })).unwrap()).unwrap();
        std::fs::write(
            path.join("lib.wasm"),
            include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm"),
        )
        .unwrap();
    }
}

fn pack(home: &Path, cwd: &Path, args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pishoo-pack"))
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}
fn checked(home: &Path, kind: &str, file: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
        .args([kind, "check"])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn fails(output: &Output, error: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "unexpected success output");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(error),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn clean(path: &Path) {
    if path.exists() {
        assert!(
            std::fs::read_dir(path).unwrap().next().is_none(),
            "staged/output file survived failure"
        );
    }
}

#[test]
fn default_app_and_repository_survive_real_host_verification() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app with spaces");
    let repository = "https://github.com/genmeta/example?ref=main#source";
    project(&app, "app", &format!("repository='{repository}'"));
    let result = success(&pack(&home, &app, &[]));
    let file = app.join("dist/hello-1.0.0-app.webc");
    assert_eq!(
        result["file"],
        std::fs::canonicalize(&file).unwrap().to_str().unwrap()
    );
    let bytes = std::fs::read(&file).unwrap();
    assert_eq!(result["size"], bytes.len());
    assert_eq!(
        result["package_sha256"],
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert_eq!(result["id"], "alice.dhttp.net/hello");
    let metadata = checked(&home, "app", &file);
    assert_eq!(metadata["repository"], repository);
    let container =
        webc::Container::from_bytes_and_version(bytes.into(), webc::Version::V3).unwrap();
    let manifest = serde_json::to_value(container.manifest()).unwrap();
    assert_eq!(manifest["package"]["wapm"]["repository"], repository);
    assert!(manifest["package"]["pishoo"].get("repository").is_none());
    assert!(std::fs::read_dir(app.join("dist")).unwrap().count() == 1);
}

#[test]
fn manifest_input_explicit_relative_output_and_normalized_id() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    let result = success(&pack(
        &home,
        dir.path(),
        &[
            app.join("wasmer.toml").as_os_str(),
            "-o".as_ref(),
            "output with spaces.webc".as_ref(),
            "--id".as_ref(),
            "ALICE~".as_ref(),
        ],
    ));
    let file = dir.path().join("output with spaces.webc");
    assert_eq!(
        result["file"],
        std::fs::canonicalize(&file).unwrap().to_str().unwrap()
    );
    assert!(checked(&home, "app", &file).get("repository").is_none());
}

#[test]
fn compatible_lib_is_signed_and_compiled_without_executing_guest() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let lib = dir.path().join("lib");
    project(&lib, "lib", "");
    let result = success(&pack(&home, &lib, &[]));
    assert_eq!(result["kind"], "lib");
    let metadata = checked(&home, "lib", &lib.join("dist/hello-1.0.0-lib.webc"));
    assert_eq!(metadata["version"], "1.0.0");
}

#[test]
fn urls_and_missing_credentials_and_id_mismatches_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    project(&app, "app", "");
    let home = dir.path().join("empty-home/.dhttp");
    for url in [
        "https://example.com/wasmer.toml",
        "https:example.com/wasmer.toml",
        "file:/tmp/wasmer.toml",
        "data:text/plain,manifest",
        "git:repository",
    ] {
        fails(&pack(&home, &app, &[url.as_ref()]), "local filesystem");
    }
    fails(
        &pack(&home, &app, &["-i".as_ref(), "bob~".as_ref()]),
        "differs from manifest publisher",
    );
    fails(&pack(&home, &app, &[]), "local credentials");
    assert!(!app.join("dist").exists());
}

#[test]
fn malformed_repository_is_rejected_before_loading_credentials() {
    for raw in [
        "''",
        "123",
        "'http://github.com/x/y'",
        "'https://user:password@example.com/a'",
        "'https://@example.com/a'",
        "'https://'",
        "'https:///path'",
        "'https://example.com/a b'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        project(&app, "app", &format!("repository={raw}"));
        let result = pack(&dir.path().join("empty-home/.dhttp"), &app, &[]);
        fails(&result, "repository");
        assert!(!app.join("dist").exists());
    }
}

#[test]
fn invalid_declarations_and_missing_artifacts_do_not_publish() {
    for (kind, from, to, error) in [
        (
            "app",
            "dependencies=[]",
            "dependencies=[{publisher='alice.dhttp.net',name='hello',version='01.0.0'}]",
            "dependency",
        ),
        (
            "app",
            "version='1.0.0'",
            "version='1.0.0-preview'",
            "MAJOR.MINOR.PATCH",
        ),
        ("app", "kind='app'", "kind='app'\nhost={}", "forbids host"),
        (
            "app",
            "[fs]",
            "[[module]]\nname='backend'\nsource='lib.wasm'\n[fs]",
            "zero backend",
        ),
        ("lib", "host={}", "entry='/'\nhost={}", "forbids entry"),
        (
            "lib",
            "dependencies=[]",
            "dependencies=[{publisher='alice.dhttp.net',name='hello',version='1.0.0'}]",
            "self Lib dependency",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("project");
        project(&app, kind, "");
        let manifest = app.join("wasmer.toml");
        std::fs::write(
            &manifest,
            std::fs::read_to_string(&manifest)
                .unwrap()
                .replace(from, to),
        )
        .unwrap();
        fails(
            &pack(&dir.path().join("empty-home/.dhttp"), &app, &[]),
            error,
        );
        assert!(!app.join("dist").exists());
    }
    for (kind, missing) in [
        ("app", "public/index.html"),
        ("lib", "metadata/openapi.json"),
        ("lib", "lib.wasm"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("project");
        project(&app, kind, "");
        std::fs::remove_file(app.join(missing)).unwrap();
        fails(
            &pack(&dir.path().join("empty-home/.dhttp"), &app, &[]),
            "build artifact",
        );
        assert!(!app.join("dist").exists());
    }
}

#[test]
fn invalid_files_and_existing_targets_preserve_old_content() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    let file = dir.path().join("old.webc");
    std::fs::write(&file, "old content").unwrap();
    fails(
        &pack(&home, &app, &["-o".as_ref(), file.as_os_str()]),
        "output already exists",
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"old content");
    for invalid in ["a%20b", "name with space", "question?", "back\\slash"] {
        let bad = app.join("public").join(invalid);
        std::fs::write(&bad, "invalid").unwrap();
        fails(&pack(&home, &app, &[]), "invalid content file path");
        std::fs::remove_file(bad).unwrap();
        assert!(!app.join("dist").exists());
    }
    let large = app.join("public/large.bin");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    fails(&pack(&home, &app, &[]), "size limit");
    assert!(!app.join("dist").exists());
}

#[test]
fn mapped_root_rejects_default_output_and_allows_external_output() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    std::fs::rename(app.join("public/index.html"), app.join("index.html")).unwrap();
    std::fs::remove_dir(app.join("public")).unwrap();
    let manifest = app.join("wasmer.toml");
    std::fs::write(
        &manifest,
        std::fs::read_to_string(&manifest)
            .unwrap()
            .replace("'/public'='public'", "'/public'='.'"),
    )
    .unwrap();
    fails(&pack(&home, &app, &[]), "inside a content mapping");
    assert!(!app.join("dist").exists());
    success(&pack(
        &home,
        &app,
        &["-o".as_ref(), dir.path().join("out.webc").as_os_str()],
    ));
}

#[test]
fn host_validation_failures_remove_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let lib = dir.path().join("lib");
    project(&lib, "lib", "");
    let api = lib.join("metadata/openapi.json");
    let original = std::fs::read(&api).unwrap();
    std::fs::write(
        &api,
        String::from_utf8(original.clone())
            .unwrap()
            .replace("1.0.0", "2.0.0"),
    )
    .unwrap();
    fails(&pack(&home, &lib, &[]), "version differs");
    clean(&lib.join("dist"));
    std::fs::write(&api, original).unwrap();
    // Component encoding is valid, but imports/exports do not satisfy WASI HTTP.
    std::fs::write(lib.join("lib.wasm"), wat::parse_str("(component)").unwrap()).unwrap();
    fails(&pack(&home, &lib, &[]), "validation failed");
    clean(&lib.join("dist"));
}

#[test]
fn tampering_with_signed_manifest_is_rejected_by_host() {
    use std::collections::BTreeMap;

    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{Directory, FileEntry, Writer},
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    success(&pack(&home, &app, &[]));
    let file = app.join("dist/hello-1.0.0-app.webc");
    let package = webc::Container::from_bytes_and_version(
        std::fs::read(&file).unwrap().into(),
        webc::Version::V3,
    )
    .unwrap();
    let mut manifest = serde_json::to_value(package.manifest()).unwrap();
    manifest["package"]["wapm"]["description"] = "Changed after signing".into();
    let manifest: webc::metadata::Manifest = serde_json::from_value(manifest).unwrap();
    let mut writer = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(BTreeMap::new())
        .unwrap();
    for (name, volume) in package.volumes() {
        let filename = if name == "public" {
            "index.html"
        } else {
            "signature.json"
        };
        let data = volume
            .read_file(format!("/{filename}").as_str())
            .unwrap()
            .0
            .to_vec();
        writer
            .write_volume(
                &name,
                Directory::new(
                    BTreeMap::from([(
                        filename.parse().unwrap(),
                        FileEntry::owned(data, Timestamps::default()).into(),
                    )]),
                    Timestamps::default(),
                ),
            )
            .unwrap();
    }
    let tampered = dir.path().join("tampered.webc");
    std::fs::write(&tampered, writer.finish(SignatureAlgorithm::None).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
        .args(["app", "check"])
        .arg(&tampered)
        .output()
        .unwrap();
    fails(&output, "signature verification failed");
}

#[cfg(unix)]
#[test]
fn symlink_aliases_cannot_hide_mapped_output_or_content() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    let alias = dir.path().join("alias");
    symlink(app.join("public"), &alias).unwrap();
    fails(
        &pack(
            &home,
            &app,
            &["-o".as_ref(), alias.join("nested/out.webc").as_os_str()],
        ),
        "inside a content mapping",
    );
    assert!(!app.join("public/nested").exists());
    symlink(app.join("public/index.html"), app.join("public/link.html")).unwrap();
    fails(&pack(&home, &app, &[]), "symlinks");
    assert!(!app.join("dist").exists());
    std::fs::remove_file(app.join("public/link.html")).unwrap();
    std::fs::rename(app.join("public"), app.join("assets")).unwrap();
    symlink(app.join("assets"), app.join("public")).unwrap();
    fails(&pack(&home, &app, &[]), "symlinks");
}

#[cfg(unix)]
#[test]
fn non_utf8_project_and_output_paths_are_passed_losslessly() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"app-\xff".to_vec()));
    // APFS rejects invalid UTF-8 filenames before Rust can open them. Verify
    // that the CLI accepts the OS argument and reports a local-path error there.
    if std::fs::create_dir(&app).is_err() {
        fails(
            &pack(&home, dir.path(), &[app.as_os_str()]),
            "missing local project",
        );
        return;
    }
    project(&app, "app", "");
    let output = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"out-\xfe.webc".to_vec()));
    success(&pack(
        &home,
        dir.path(),
        &[app.as_os_str(), "-o".as_ref(), output.as_os_str()],
    ));
    assert!(output.is_file());
    checked(&home, "app", &output);
}

#[cfg(unix)]
#[test]
fn concurrent_target_creation_cannot_be_overwritten() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    let tools = dir.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let packer = tools.join("pishoo-pack");
    std::fs::copy(env!("CARGO_BIN_EXE_pishoo-pack"), &packer).unwrap();
    let validator = tools.join("pishoo");
    // Hold the actual validator after validation so the test deterministically
    // creates a destination between the early existence check and publication.
    std::fs::write(&validator, "#!/bin/sh\n\"$REAL_PISHOO\" \"$@\" > \"$BARRIER/result\" || exit 1\n: > \"$BARRIER/ready\"\nwhile [ ! -f \"$BARRIER/release\" ]; do sleep 0.02; done\ncat \"$BARRIER/result\"\n").unwrap();
    std::fs::set_permissions(&validator, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output_dir = dir.path().join("out");
    let target = output_dir.join("race.webc");
    let child = Command::new(packer)
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
        .env("REAL_PISHOO", env!("CARGO_BIN_EXE_pishoo"))
        .env("BARRIER", dir.path())
        .arg(&app)
        .arg("-o")
        .arg(&target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !dir.path().join("ready").is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "validator did not reach barrier"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(&target, "concurrent creator").unwrap();
    std::fs::write(dir.path().join("release"), "").unwrap();
    fails(&child.wait_with_output().unwrap(), "output already exists");
    assert_eq!(std::fs::read(&target).unwrap(), b"concurrent creator");
    assert_eq!(std::fs::read_dir(output_dir).unwrap().count(), 1);
}

#[test]
fn help_version_and_legacy_example_contract_work() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("app");
    project(&app, "app", "");
    for flag in ["--help", "-h", "--version", "-V"] {
        let output = pack(&home, &app, &[flag.as_ref()]);
        assert!(output.status.success());
    }
    let output_file = dir.path().join("legacy.webc");
    success(&pack(
        &home,
        &app,
        &[
            app.join("wasmer.toml").as_os_str(),
            output_file.as_os_str(),
            "--publisher".as_ref(),
            "alice~".as_ref(),
        ],
    ));
    checked(&home, "app", &output_file);
    for flag in ["--force", "--unsigned", "--kind", "--home", "--upload"] {
        fails(&pack(&home, &app, &[flag.as_ref()]), "unexpected argument");
    }
}

#[cfg(unix)]
#[test]
fn backend_fifo_is_rejected_without_waiting_for_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("lib");
    project(&lib, "lib", "");
    let backend = lib.join("lib.wasm");
    std::fs::remove_file(&backend).unwrap();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&backend)
            .status()
            .unwrap()
            .success()
    );
    fails(
        &pack(&dir.path().join("empty-home/.dhttp"), &lib, &[]),
        "regular files",
    );
    assert!(!lib.join("dist").exists());
}

#[test]
fn installed_tool_and_validator_need_no_source_checkout_or_cargo() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home/.dhttp");
    identity(&home);
    let app = dir.path().join("project");
    project(&app, "app", "");
    let bin = dir.path().join("installed/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let packer = bin.join(format!("pishoo-pack{}", std::env::consts::EXE_SUFFIX));
    let validator = bin.join(format!("pishoo{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_pishoo-pack"), &packer).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_pishoo"), &validator).unwrap();
    let mut command = if let Some(launcher) = std::env::var_os("GENMETA_PACK_TEST_BIN") {
        let installed = bin.join(format!("genmeta{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(launcher, &installed).unwrap();
        let mut command = Command::new(installed);
        command.arg("pack");
        command
    } else {
        Command::new(packer)
    };
    let output = command
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
        .env("PATH", "")
        .current_dir(dir.path())
        .arg(&app)
        .output()
        .unwrap();
    success(&output);
    assert!(app.join("dist/hello-1.0.0-app.webc").is_file());
}

#[test]
fn host_rejects_invalid_repository_even_in_independently_built_packages() {
    use std::collections::BTreeMap;

    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{Directory, Writer},
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("invalid.webc");
    for repository in [
        json!(null),
        json!(42),
        json!(""),
        json!("http://example.com"),
        json!("https://user:password@example.com"),
        json!("https://@example.com"),
        json!("https:///path"),
    ] {
        let manifest: webc::metadata::Manifest = serde_json::from_value(json!({"package":{
            "wapm":{"name":"alice.dhttp.net/hello","version":"1.0.0","description":"Test","license":"MIT","repository":repository},
            "pishoo":{"kind":"app","title":"Hello","entry":"/","dependencies":[],"changelog":"Test"},
            "fs":[{"volume_name":"public","mount_path":"/public"}]}})).unwrap();
        let bytes = Writer::default()
            .write_manifest(&manifest)
            .unwrap()
            .write_atoms(BTreeMap::new())
            .unwrap()
            .with_volume(
                "public",
                Directory::new(
                    BTreeMap::from([("index.html".parse().unwrap(), vec![1].into())]),
                    Timestamps::default(),
                ),
            )
            .unwrap()
            .finish(SignatureAlgorithm::None)
            .unwrap();
        std::fs::write(&file, bytes).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
            .env("DHTTP_HOME", dir.path())
            .args(["app", "check"])
            .arg(&file)
            .output()
            .unwrap();
        fails(&output, "repository");
    }
}
