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
        "0.8.0~beta.6-1"
    );
    assert_eq!(
        PackageVersion::rpm(common.source_version, rpm.release.clone())
            .expect("pishoo-common rpm version should compose")
            .as_string(),
        "0.8.0~beta.6-1"
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
        assert_eq!(common.maximum.as_deref(), Some("0.8.0~beta.6-1"));

        let entries = linux_requirement_entries(system, "pishoo-common", common.clone())
            .expect("pishoo-common requirement entries should render");
        let expected = match system {
            PackageSystem::Deb => vec![
                "pishoo-common (>= 0.5.1-1)",
                "pishoo-common (<= 0.8.0~beta.6-1)",
            ],
            PackageSystem::Rpm => vec![
                "pishoo-common >= 0.5.1-1",
                "pishoo-common <= 0.8.0~beta.6-1",
            ],
            PackageSystem::Brew | PackageSystem::Scoop => unreachable!(),
        };
        assert_eq!(entries, expected);
    }
}
