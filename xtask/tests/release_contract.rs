use std::path::{Path, PathBuf};

use genmeta_xtask_release::{
    contract::{
        ReleaseContract, VersionBoundSource, VersionBoundSourceContract, load_release_contract,
    },
    package::{PackageVersion, resolve_metadata},
    requires::{linux_requirement_entries, resolve_requires_for},
    system::PackageSystem,
};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask should live under the repository root")
        .to_path_buf()
}

fn release_contract(root: &Path) -> ReleaseContract {
    load_release_contract(&root.join("xtask/release.toml"))
        .expect("gateway release contract should load")
}

#[test]
fn release_builds_use_the_committed_lockfile() {
    let root = repository_root();
    let cargo_config = std::fs::read_to_string(root.join(".cargo/config.toml"))
        .expect("cargo config should be readable");
    assert!(
        cargo_config.contains("xtask = \"run --locked"),
        "cargo xtask must use the xtask lockfile"
    );

    for (path, command) in [
        ("xtask/release/brew/pishoo.sh", "args=(build --locked"),
        ("xtask/release/rpm/package.sh", "cargo zigbuild --locked"),
        ("xtask/deb/rules", "cargo zigbuild --locked"),
    ] {
        let contents = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("{path} should be readable: {error}"));
        assert!(
            contents.contains(command),
            "{path} must build release artifacts with --locked"
        );
    }
}

#[test]
fn release_build_uses_canonical_dhttp_environment() {
    let root = repository_root();
    let contract = release_contract(&root);
    let package = contract.package("pishoo").expect("pishoo should exist");
    let names = package
        .build
        .env
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "DHTTP_BOOTSTRAP_URL",
            "DHTTP_CA_SERVICE",
            "DHTTP_GLOBAL_HOME",
            "DHTTP_MDNS_SERVICE_DOMAIN",
            "DHTTP_NAME_SERVICE",
            "DHTTP_ROOT_CA_PEM",
        ]
    );

    let root_ca = package
        .build
        .env
        .get("DHTTP_ROOT_CA_PEM")
        .expect("root CA PEM binding should exist");
    assert_eq!(root_ca.env.as_deref(), Some("DHTTP_ROOT_CA_PEM"));
    assert!(
        root_ca.container_path.is_none(),
        "PEM content must be passed directly instead of mounted as a path"
    );
}

#[test]
fn deb_packaging_disables_sparse_binary_copies() {
    let rules = std::fs::read_to_string(repository_root().join("xtask/deb/rules"))
        .expect("deb rules should be readable");

    for binary in ["pishoo", "pishoo-worker", "pishoo-ssh-session"] {
        let copy =
            format!("cp --sparse=never $(SOURCE_ROOT)/target/$(TRIPLE)/$(BUILD_PROFILE)/{binary}");
        assert!(
            rules.contains(&copy),
            "missing non-sparse copy for {binary}"
        );
    }
}

#[test]
fn pishoo_common_package_version_follows_pishoo() {
    let root = repository_root();
    let contract = release_contract(&root);
    let pishoo =
        resolve_metadata(&contract, "pishoo", &root).expect("pishoo metadata should resolve");
    let common = resolve_metadata(&contract, "pishoo-common", &root)
        .expect("pishoo-common metadata should resolve");

    assert_eq!(common.source_version, pishoo.source_version);

    let common_contract = contract
        .package("pishoo-common")
        .expect("pishoo-common contract should exist");
    let deb = common_contract
        .deb
        .as_ref()
        .expect("pishoo-common should have a deb branch");
    let rpm = common_contract
        .rpm
        .as_ref()
        .expect("pishoo-common should have an rpm branch");

    assert_eq!(
        PackageVersion::deb(common.source_version.clone(), deb.revision.clone())
            .expect("pishoo-common deb version should compose")
            .as_string(),
        "0.8.2~beta.1-1"
    );
    assert_eq!(
        PackageVersion::rpm(common.source_version, rpm.release.clone())
            .expect("pishoo-common rpm version should compose")
            .as_string(),
        "0.8.2~beta.1-1"
    );
}

#[test]
fn pishoo_linux_requirements_keep_published_floor_and_current_ceiling() {
    let root = repository_root();
    let contract = release_contract(&root);
    let pishoo = contract.package("pishoo").expect("pishoo should exist");

    for (system, branch) in [
        (
            PackageSystem::Deb,
            pishoo
                .deb
                .as_ref()
                .expect("pishoo should have a deb branch")
                .requires
                .get("pishoo-common")
                .expect("pishoo deb should require pishoo-common"),
        ),
        (
            PackageSystem::Rpm,
            pishoo
                .rpm
                .as_ref()
                .expect("pishoo should have an rpm branch")
                .requires
                .get("pishoo-common")
                .expect("pishoo rpm should require pishoo-common"),
        ),
    ] {
        assert_eq!(
            branch.version.minimum,
            Some(VersionBoundSourceContract::Literal("0.5.1-1".to_owned()))
        );
        assert_eq!(
            branch.version.maximum,
            Some(VersionBoundSourceContract::Source(
                VersionBoundSource::SelfPackage
            ))
        );

        let requirements = resolve_requires_for(&contract, &root, "pishoo", system)
            .expect("pishoo requirements should resolve");
        let common = requirements
            .get("pishoo-common")
            .expect("pishoo-common bounds should resolve");
        assert_eq!(common.minimum.as_deref(), Some("0.5.1-1"));
        assert_eq!(common.maximum.as_deref(), Some("0.8.2~beta.1-1"));

        let entries = linux_requirement_entries(system, "pishoo-common", common.clone())
            .expect("pishoo-common requirement entries should render");
        let expected = match system {
            PackageSystem::Deb => vec![
                "pishoo-common (>= 0.5.1-1)",
                "pishoo-common (<= 0.8.2~beta.1-1)",
            ],
            PackageSystem::Rpm => vec![
                "pishoo-common >= 0.5.1-1",
                "pishoo-common <= 0.8.2~beta.1-1",
            ],
            PackageSystem::Brew | PackageSystem::Scoop => unreachable!(),
        };
        assert_eq!(entries, expected);
    }
}
