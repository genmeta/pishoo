use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const POSTINST: &str = include_str!("../deb/pishoo-common.postinst");
const POSTRM: &str = include_str!("../deb/pishoo-common.postrm");
static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pishoo-service-install-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn service_override(&self) -> PathBuf {
        self.path()
            .join("etc/systemd/system/pishoo.service.d/identity-home.conf")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(self.path()).unwrap();
    }
}

fn executable(directory: &Path, name: &str, body: &str) {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_hook(root: &TestDir, script: &str, user: &str, home: &str, argument: &str) -> Output {
    let bin = root.path().join("bin");
    executable(
        &bin,
        "getent",
        "test \"$1\" = passwd\ncase \"$2\" in missing) exit 42 ;; esac\nprintf '%s:x:1000:1000::%s:/bin/sh\\n' \"$2\" \"$ACCOUNT_HOME\"",
    );
    let script = script
        .replace(
            "/etc/systemd/system",
            &root.path().join("etc/systemd/system").to_string_lossy(),
        )
        .replace(
            "/etc/dhttp",
            &root.path().join("etc/dhttp").to_string_lossy(),
        );
    Command::new("/bin/sh")
        .args(["-eu", "-c", &script, "package-hook", argument])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SUDO_USER", user)
        .env("ACCOUNT_HOME", home)
        .output()
        .unwrap()
}

fn rpm_hooks() -> (String, String) {
    let root = TestDir::new();
    let bin = root.path().join("bin");
    let out = root.path().join("out");
    executable(&bin, "install", "exit 0");
    executable(&bin, "rpmbuild", "exit 0");
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("/bin/bash")
        .arg(xtask.join("release/rpm/package.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XTASK_RELEASE_PACKAGE_ID", "pishoo-common")
        .env("XTASK_RELEASE_PACKAGE_VERSION", "0.9.0.preview.1-1")
        .env("XTASK_RELEASE_SOURCE_VERSION", "0.9.0-preview.1")
        .env("XTASK_RELEASE_REPO_ROOT", xtask.parent().unwrap())
        .env("XTASK_RELEASE_OUT_DIR", &out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let spec = fs::read_to_string(out.join("rpmbuild/SPECS/pishoo-common.spec")).unwrap();
    assert!(
        !spec.contains(".webc"),
        "host RPM must not bundle business packages"
    );
    assert!(
        !spec.contains("gmutils"),
        "host installation is independent of gmutils"
    );
    let setup = spec
        .split_once("\n%post\n")
        .unwrap()
        .1
        .split_once("\n%systemd_post pishoo.service")
        .unwrap()
        .0
        .replace("%%", "%");
    let cleanup = spec
        .split_once("\n%postun\n")
        .unwrap()
        .1
        .split_once("\n%systemd_postun_with_restart pishoo.service")
        .unwrap()
        .0
        .replace("%%", "%");
    (setup, cleanup)
}

#[test]
fn deb_common_packages_without_business_artifacts_or_gmutils() {
    let root = TestDir::new();
    let bin = root.path().join("bin");
    let out = root.path().join("out");
    executable(&bin, "dpkg-buildpackage", "test \"$1\" = -A");
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("/bin/bash")
        .arg(xtask.join("release/deb/package.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XTASK_RELEASE_PACKAGE_ID", "pishoo-common")
        .env("XTASK_RELEASE_PACKAGE_VERSION", "0.9.0~preview.1-1")
        .env("XTASK_RELEASE_SOURCE_VERSION", "0.9.0-preview.1")
        .env("XTASK_RELEASE_TARGET", "common")
        .env("XTASK_RELEASE_PROFILE", "release")
        .env("XTASK_RELEASE_REPO_ROOT", xtask.parent().unwrap())
        .env("XTASK_RELEASE_OUT_DIR", &out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let debian = out.join("src/debian");
    assert!(debian.join("pishoo-common.pishoo.service").is_file());
    assert!(debian.join("common/etc/dhttp/mime.types").is_file());
    assert!(!debian.join("common/usr/share/pishoo").exists());
    let control = fs::read_to_string(debian.join("control")).unwrap();
    assert!(!control.contains("gmutils"));
    assert!(!control.contains("{{PISHOO_COMMON_DEPENDS}}"));
}

#[test]
fn brew_archive_packages_only_host_binaries_without_business_artifacts() {
    let root = TestDir::new();
    let bin = root.path().join("bin");
    let out = root.path().join("out");
    let source = root.path().join("source");
    fs::create_dir_all(source.join("pishoo")).unwrap();
    fs::write(
        source.join("Cargo.toml"),
        "[workspace]\nmembers = [\"gateway\", \"pishoo\"]\n",
    )
    .unwrap();
    executable(
        &bin,
        "cargo",
        r#"
test "$1" = build
while [ "$#" -gt 0 ]; do
    case "$1" in
        --manifest-path) shift; manifest=$1 ;;
        --target) shift; target=$1 ;;
    esac
    shift
done
release="$(dirname "$(dirname "$manifest")")/target/$target/release"
mkdir -p "$release"
for binary in pishoo; do
    printf '#!/bin/sh\nexit 0\n' > "$release/$binary"
    chmod +x "$release/$binary"
done
"#,
    );
    let output = Command::new("/bin/bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("release/brew/pishoo.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XTASK_RELEASE_SOURCE_VERSION", "0.9.0-preview.1")
        .env("XTASK_RELEASE_TARGET", "aarch64-apple-darwin")
        .env("XTASK_RELEASE_REPO_ROOT", &source)
        .env("XTASK_RELEASE_OUT_DIR", &out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let archive = out.join("pishoo_0.9.0-preview.1-aarch64-apple-darwin.tar.gz");
    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let mut files: Vec<_> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.ends_with('/'))
        .map(str::to_owned)
        .collect();
    files.sort();
    assert_eq!(files, ["./pishoo"]);
    let formula =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/pishoo.rb.in"))
            .unwrap();
    assert!(!formula.contains("default-artifacts"));
    assert!(!formula.contains("depends_on \"gmutils\""));
    assert!(formula.contains("genmeta install workspace"));
}

#[test]
fn deb_and_rpm_use_the_root_unit_without_binding_to_the_installer() {
    let unit = include_str!("../deb/pishoo-common.pishoo.service");
    assert!(unit.contains("User=root\n"));
    assert!(!unit.contains("DHTTP_HOME="));
    let (rpm_setup, _) = rpm_hooks();
    for script in [POSTINST, &rpm_setup] {
        for user in ["alice", "root", ""] {
            let root = TestDir::new();
            let output = run_hook(&root, script, user, "/home/ignored", "configure");
            assert!(output.status.success(), "{output:?}");
            assert!(!root.service_override().exists());
        }
    }
}

#[test]
fn upgrades_retire_only_the_unchanged_generated_single_user_override() {
    let (rpm_setup, _) = rpm_hooks();
    for script in [POSTINST, &rpm_setup] {
        let root = TestDir::new();
        fs::create_dir_all(root.service_override().parent().unwrap()).unwrap();
        fs::write(root.service_override(), "# Generated by pishoo-common.\n[Service]\nUser=alice\nEnvironment=\"DHTTP_HOME=/home/alice/.dhttp\"\n").unwrap();
        assert!(
            run_hook(&root, script, "bob", "/home/bob", "configure")
                .status
                .success()
        );
        assert!(!root.service_override().exists());
        // Idempotent upgrade, without account lookups.
        assert!(
            run_hook(&root, script, "missing", "relative", "configure")
                .status
                .success()
        );
        assert!(!root.service_override().exists());
    }
}

#[test]
fn administrator_overrides_and_symlinks_are_preserved_on_upgrade() {
    let (rpm_setup, _) = rpm_hooks();
    for script in [POSTINST, &rpm_setup] {
        for name in ["identity-home.conf", "custom.conf"] {
            for original in [
                "[Service]\nUser=service-owner\nEnvironment=DHTTP_HOME=/srv/private\n",
                "# Generated by pishoo-common.\n[Service]\nUser=alice\nEnvironment=\"DHTTP_HOME=/home/alice/.dhttp\"\nMemoryMax=1G\n",
            ] {
                let root = TestDir::new();
                let directory = root.service_override().parent().unwrap().to_path_buf();
                fs::create_dir_all(&directory).unwrap();
                let path = directory.join(name);
                fs::write(&path, original).unwrap();
                assert!(
                    run_hook(&root, script, "alice", "/home/alice", "configure")
                        .status
                        .success()
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), original);
            }
        }
        let root = TestDir::new();
        fs::create_dir_all(root.service_override().parent().unwrap()).unwrap();
        symlink("missing-admin-file", root.service_override()).unwrap();
        assert!(
            run_hook(&root, script, "alice", "/home/alice", "configure")
                .status
                .success()
        );
        assert_eq!(
            fs::read_link(root.service_override()).unwrap(),
            Path::new("missing-admin-file")
        );
    }
}

#[test]
fn purge_preserves_unmanaged_service_overrides() {
    let root = TestDir::new();
    fs::create_dir_all(root.service_override().parent().unwrap()).unwrap();
    let original = "[Service]\nUser=custom-owner\n";
    fs::write(root.service_override(), original).unwrap();
    assert!(
        run_hook(&root, POSTRM, "", "/root", "purge")
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(root.service_override()).unwrap(),
        original
    );
}

#[test]
fn uninstall_removes_only_unchanged_legacy_package_overrides() {
    let (_, rpm_cleanup) = rpm_hooks();
    for (script, keep, remove) in [
        (POSTRM, "remove", "purge"),
        (rpm_cleanup.as_str(), "1", "0"),
    ] {
        let root = TestDir::new();
        fs::create_dir_all(root.service_override().parent().unwrap()).unwrap();
        let original = "# Generated by pishoo-common.\n[Service]\nUser=alice\nEnvironment=\"DHTTP_HOME=/home/alice/.dhttp\"\n";
        fs::write(root.service_override(), original).unwrap();
        assert!(run_hook(&root, script, "", "/root", keep).status.success());
        assert!(root.service_override().exists());
        assert!(
            run_hook(&root, script, "", "/root", remove)
                .status
                .success()
        );
        assert!(!root.service_override().exists());
        let customized = format!("{original}MemoryMax=1G\n");
        fs::create_dir_all(root.service_override().parent().unwrap()).unwrap();
        fs::write(root.service_override(), &customized).unwrap();
        assert!(
            run_hook(&root, script, "", "/root", remove)
                .status
                .success()
        );
        assert_eq!(
            fs::read_to_string(root.service_override()).unwrap(),
            customized
        );
    }
}
