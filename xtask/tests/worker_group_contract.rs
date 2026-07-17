use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const DEB_POSTINST: &str = include_str!("../deb/pishoo-common.postinst");
const DEB_CONTROL: &str = include_str!("../deb/control");
const RPM_PACKAGE_SCRIPT: &str = include_str!("../release/rpm/package.sh");

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct GroupHook {
    name: &'static str,
    script: String,
    configure_argument: bool,
    create_invocation: &'static str,
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pishoo-{label}-{}-{sequence}", std::process::id()));
        fs::create_dir(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn generated_rpm_preinstall() -> String {
    let test_dir = TestDir::new("rpm-spec");
    let bin = test_dir.path().join("bin");
    let out = test_dir.path().join("out");
    fs::create_dir(&bin).expect("fake command directory should be created");
    write_fake_command(&bin, "rpmbuild", "exit 0");

    let xtask_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repository_root = xtask_root
        .parent()
        .expect("xtask should live under the repository root");
    let path = std::env::var_os("PATH").expect("tests should have PATH");
    let output = Command::new("/bin/bash")
        .arg(xtask_root.join("release/rpm/package.sh"))
        .env(
            "PATH",
            format!("{}:{}", bin.display(), path.to_string_lossy()),
        )
        .env("XTASK_RELEASE_PACKAGE_ID", "pishoo-common")
        .env("XTASK_RELEASE_PACKAGE_VERSION", "0.8.0~beta.6-1")
        .env("XTASK_RELEASE_SOURCE_VERSION", "0.8.0-beta.6")
        .env("XTASK_RELEASE_REPO_ROOT", repository_root)
        .env("XTASK_RELEASE_OUT_DIR", &out)
        .output()
        .expect("rpm package script should execute");
    assert!(
        output.status.success(),
        "rpm common spec generation failed: {output:?}"
    );

    let spec = fs::read_to_string(out.join("rpmbuild/SPECS/pishoo-common.spec"))
        .expect("rpm common spec should be generated");
    spec.split_once("\n%pre\n")
        .expect("rpm common spec should define %pre")
        .1
        .split_once("\n%post\n")
        .expect("rpm common spec should define %post after %pre")
        .0
        .to_owned()
}

fn group_hooks() -> [GroupHook; 2] {
    [
        GroupHook {
            name: "deb",
            script: DEB_POSTINST.to_owned(),
            configure_argument: true,
            create_invocation: "addgroup --system --quiet dhttp\n",
        },
        GroupHook {
            name: "rpm",
            script: generated_rpm_preinstall(),
            configure_argument: false,
            create_invocation: "groupadd --system dhttp\n",
        },
    ]
}

fn write_fake_command(bin: &Path, name: &str, body: &str) {
    let path = bin.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n"))
        .expect("fake command should be written");
    let mut permissions = fs::metadata(&path)
        .expect("fake command metadata should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("fake command should be executable");
}

fn run_group_hook(hook: &GroupHook, getent_status: i32, create_status: i32) -> (Output, String) {
    let test_dir = TestDir::new(hook.name);
    let bin = test_dir.path().join("bin");
    fs::create_dir(&bin).expect("fake command directory should be created");
    let log = test_dir.path().join("commands.log");

    write_fake_command(
        &bin,
        "getent",
        "printf 'getent %s\\n' \"$*\" >> \"$HOOK_LOG\"\nexit \"$GETENT_STATUS\"",
    );
    for command in ["addgroup", "groupadd"] {
        write_fake_command(
            &bin,
            command,
            &format!("printf '{command} %s\\n' \"$*\" >> \"$HOOK_LOG\"\nexit \"$CREATE_STATUS\""),
        );
    }
    write_fake_command(
        &bin,
        "install",
        "printf 'install %s\\n' \"$*\" >> \"$HOOK_LOG\"",
    );

    let mut command = Command::new("/bin/sh");
    command
        .arg("-eu")
        .arg("-c")
        .arg(&hook.script)
        .arg(hook.name)
        .env_clear()
        .env("PATH", &bin)
        .env("HOOK_LOG", &log)
        .env("GETENT_STATUS", getent_status.to_string())
        .env("CREATE_STATUS", create_status.to_string());
    if hook.configure_argument {
        command.arg("configure");
    }
    let output = command.output().expect("group hook should execute");
    let commands = fs::read_to_string(log).unwrap_or_default();
    (output, commands)
}

#[test]
fn linux_group_hooks_create_the_dhttp_group_without_masking_failures() {
    assert!(DEB_POSTINST.contains("addgroup --system --quiet dhttp"));
    assert!(!DEB_POSTINST.contains("addgroup --system --quiet pishoo"));
    assert!(
        !DEB_POSTINST.lines().any(|line| {
            line.contains("addgroup --system --quiet dhttp") && line.contains("||")
        })
    );
    assert!(RPM_PACKAGE_SCRIPT.contains("groupadd --system dhttp"));
    assert!(!RPM_PACKAGE_SCRIPT.contains("groupadd --system pishoo"));
    assert!(
        !RPM_PACKAGE_SCRIPT
            .lines()
            .any(|line| line.contains("groupadd --system dhttp") && line.contains("||"))
    );
}

#[test]
fn linux_common_packages_depend_on_their_group_creation_tools() {
    let common = DEB_CONTROL
        .split_once("Package: pishoo-common\n")
        .expect("debian control should define pishoo-common")
        .1
        .split_once("\n\nPackage:")
        .expect("pishoo-common should be followed by another package")
        .0;

    assert!(
        common
            .lines()
            .any(|line| line == "Depends: adduser, ${misc:Depends}")
    );
    assert!(RPM_PACKAGE_SCRIPT.contains("Requires(pre):  shadow-utils"));
}

#[test]
fn linux_group_hooks_skip_creation_when_dhttp_already_exists() {
    for hook in group_hooks() {
        let (output, commands) = run_group_hook(&hook, 0, 91);
        assert!(
            output.status.success(),
            "{} hook failed: {output:?}",
            hook.name
        );
        assert!(commands.contains("getent group dhttp\n"), "{commands}");
        assert!(!commands.contains("addgroup "), "{commands}");
        assert!(!commands.contains("groupadd "), "{commands}");
    }
}

#[test]
fn linux_group_hooks_create_dhttp_when_it_is_absent() {
    for hook in group_hooks() {
        let (output, commands) = run_group_hook(&hook, 2, 0);
        assert!(
            output.status.success(),
            "{} hook failed: {output:?}",
            hook.name
        );
        assert!(commands.contains("getent group dhttp\n"), "{commands}");
        assert!(commands.contains(hook.create_invocation), "{commands}");
    }
}

#[test]
fn linux_group_hooks_propagate_group_creation_failures() {
    for hook in group_hooks() {
        let (output, commands) = run_group_hook(&hook, 2, 42);
        assert_eq!(
            output.status.code(),
            Some(42),
            "{} hook: {output:?}",
            hook.name
        );
        assert!(commands.contains(hook.create_invocation), "{commands}");
    }
}

#[test]
fn linux_group_hooks_propagate_getent_failures() {
    for hook in group_hooks() {
        let (output, commands) = run_group_hook(&hook, 3, 0);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{} hook: {output:?}",
            hook.name
        );
        assert!(commands.contains("getent group dhttp\n"), "{commands}");
        assert!(!commands.contains("addgroup "), "{commands}");
        assert!(!commands.contains("groupadd "), "{commands}");
    }
}
