use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use cargo_metadata::MetadataCommand;
use serde::Deserialize;
use snafu::{ResultExt, Snafu};

use crate::version_cmp::{CompareVersionError, compare_deb_versions};

const RELEASE_CONTRACT_PATH: &str = "xtask/release.toml";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReleaseContract {
    pub cargo: CargoSource,
    pub package: PackageContract,
    pub homebrew: Option<HomebrewContract>,
    pub build: BuildContract,
    pub destination: DestinationContract,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CargoSource {
    pub manifest: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PackageContract {
    pub common: CommonPackageContract,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CommonPackageContract {
    pub name: String,
    pub version: String,
    pub required_version: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HomebrewContract {
    pub template: TemplateContract,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TemplateContract {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BuildContract {
    pub env: BuildEnvContract,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BuildEnvContract {
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DestinationContract {
    pub s3: S3Destination,
    pub brew: Option<BrewDestination>,
    pub deb: Option<DebDestination>,
    pub rpm: Option<RpmDestination>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EnvRef {
    pub env: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct S3Destination {
    pub bucket: String,
    pub endpoint: EnvRef,
    pub access_key_id: EnvRef,
    pub secret_access_key: EnvRef,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BrewDestination {
    pub prefix: String,
    pub public_base_url: String,
    pub tap: TapDestination,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TapDestination {
    pub repository: String,
    pub base_branch: String,
    pub token: EnvRef,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DebDestination {
    pub prefix: String,
    pub suite: String,
    pub signing: DebSigning,
    pub fingerprint: Option<EnvRef>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DebSigning {
    pub key: EnvRef,
    pub passphrase: EnvRef,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RpmDestination {
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackageMetadata {
    pub name: String,
    pub version: String,
    pub description: String,
    pub homepage: String,
    pub license: String,
    pub repository: Option<String>,
    pub authors: Vec<String>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ReleaseContractError {
    #[snafu(display("failed to read release contract"))]
    Read {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse release contract"))]
    Parse {
        source: toml::de::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to read cargo metadata"))]
    CargoMetadata {
        source: cargo_metadata::Error,
        manifest: PathBuf,
    },
    #[snafu(display("cargo metadata did not return a root package"))]
    MissingRootPackage { manifest: PathBuf },
    #[snafu(display("cargo package is missing description"))]
    MissingDescription { manifest: PathBuf },
    #[snafu(display("cargo package is missing homepage"))]
    MissingHomepage { manifest: PathBuf },
    #[snafu(display("cargo package is missing license"))]
    MissingLicense { manifest: PathBuf },
    #[snafu(display("failed to compare release contract versions"))]
    Compare { source: CompareVersionError },
    #[snafu(display(
        "required common version {required_common_version} exceeds common version {common_version}"
    ))]
    RequiredExceedsCommon {
        common_version: String,
        required_common_version: String,
    },
    #[snafu(display("missing required build environment variable {name}"))]
    MissingBuildEnv { name: String },
    #[snafu(display("build environment variable {name} must not be empty"))]
    EmptyBuildEnv { name: String },
}

#[cfg(test)]
fn parse_release_contract(input: &str) -> Result<ReleaseContract, ReleaseContractError> {
    parse_release_contract_at(Path::new(RELEASE_CONTRACT_PATH), input)
}

pub fn load_release_contract() -> Result<ReleaseContract, ReleaseContractError> {
    read_release_contract(&default_release_contract_path())
}

fn default_release_contract_path() -> PathBuf {
    let cwd_path = PathBuf::from(RELEASE_CONTRACT_PATH);
    if cwd_path.exists() {
        return cwd_path;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("release.toml")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest directory should have a parent")
        .to_path_buf()
}

pub fn read_release_contract(path: &Path) -> Result<ReleaseContract, ReleaseContractError> {
    let input = std::fs::read_to_string(path).context(release_contract_error::ReadSnafu {
        path: path.to_path_buf(),
    })?;
    parse_release_contract_at(path, &input)
}

fn parse_release_contract_at(
    path: &Path,
    input: &str,
) -> Result<ReleaseContract, ReleaseContractError> {
    let contract = toml::from_str(input).context(release_contract_error::ParseSnafu {
        path: path.to_path_buf(),
    })?;
    validate_release_contract(&contract)?;
    Ok(contract)
}

fn validate_release_contract(contract: &ReleaseContract) -> Result<(), ReleaseContractError> {
    if compare_deb_versions(
        &contract.package.common.required_version,
        &contract.package.common.version,
    )
    .context(release_contract_error::CompareSnafu)?
    .is_gt()
    {
        return Err(ReleaseContractError::RequiredExceedsCommon {
            common_version: contract.package.common.version.clone(),
            required_common_version: contract.package.common.required_version.clone(),
        });
    }
    Ok(())
}

pub fn resolve_package_metadata(
    contract: &ReleaseContract,
) -> Result<ResolvedPackageMetadata, ReleaseContractError> {
    let manifest = if contract.cargo.manifest.is_absolute() {
        contract.cargo.manifest.clone()
    } else {
        repo_root().join(&contract.cargo.manifest)
    };
    let metadata = MetadataCommand::new()
        .manifest_path(&manifest)
        .no_deps()
        .exec()
        .context(release_contract_error::CargoMetadataSnafu {
            manifest: manifest.clone(),
        })?;
    let package =
        metadata
            .root_package()
            .ok_or_else(|| ReleaseContractError::MissingRootPackage {
                manifest: manifest.clone(),
            })?;
    Ok(ResolvedPackageMetadata {
        name: package.name.to_string(),
        version: package.version.to_string(),
        description: package.description.clone().ok_or_else(|| {
            ReleaseContractError::MissingDescription {
                manifest: manifest.clone(),
            }
        })?,
        homepage: package.homepage.clone().ok_or_else(|| {
            ReleaseContractError::MissingHomepage {
                manifest: manifest.clone(),
            }
        })?,
        license: package
            .license
            .clone()
            .ok_or_else(|| ReleaseContractError::MissingLicense {
                manifest: manifest.clone(),
            })?,
        repository: package.repository.clone(),
        authors: package.authors.clone(),
    })
}

pub fn validate_required_build_env(contract: &ReleaseContract) -> Result<(), ReleaseContractError> {
    let values = std::env::vars().collect::<BTreeMap<_, _>>();
    validate_required_build_env_values(contract, &values)
}

pub fn validate_required_build_env_values(
    contract: &ReleaseContract,
    values: &BTreeMap<String, String>,
) -> Result<(), ReleaseContractError> {
    for name in &contract.build.env.required {
        match values.get(name) {
            Some(value) if value.is_empty() => {
                return Err(ReleaseContractError::EmptyBuildEnv { name: name.clone() });
            }
            Some(_) => {}
            None => {
                return Err(ReleaseContractError::MissingBuildEnv { name: name.clone() });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{ReleaseContract, parse_release_contract, validate_required_build_env_values};

    const CONTRACT: &str = r#"
[cargo]
manifest = "pishoo/Cargo.toml"

[homebrew.template]
path = "xtask/templates/pishoo.rb.in"

[build.env]
required = ["DHTTP_ROOT_CA", "DHTTP_STUN_SERVER"]

[destination.s3]
bucket = "download"
endpoint.env = "XTASK_RELEASE_S3_ENDPOINT_URL"
access_key_id.env = "XTASK_RELEASE_S3_ACCESS_KEY_ID"
secret_access_key.env = "XTASK_RELEASE_S3_SECRET_ACCESS_KEY"

[destination.brew]
prefix = "brew/pishoo"
public_base_url = "https://download.dhttp.net/brew/pishoo"
tap.repository = "genmeta/homebrew-genmeta"
tap.base_branch = "main"
tap.token.env = "HOMEBREW_TAP_GITHUB_TOKEN"

[destination.deb]
prefix = "ppa/genmeta"
suite = "genmeta"
signing.key.env = "XTASK_RELEASE_APT_SIGNING_KEY"
signing.passphrase.env = "XTASK_RELEASE_APT_SIGNING_PASSPHRASE"

[destination.rpm]
prefix = "rpm/pishoo"

[package.common]
name = "pishoo-common"
version = "0.5.2-1"
required_version = "0.5.1-1"
"#;

    #[test]
    fn parses_release_contract() {
        let contract = parse_release_contract(CONTRACT).expect("contract should parse");

        assert_eq!(
            contract.cargo.manifest,
            std::path::Path::new("pishoo/Cargo.toml")
        );
        assert_eq!(contract.package.common.name, "pishoo-common");
        assert_eq!(contract.package.common.version, "0.5.2-1");
        assert_eq!(contract.package.common.required_version, "0.5.1-1");
        assert_eq!(contract.destination.rpm.unwrap().prefix, "rpm/pishoo");
    }

    #[test]
    fn rejects_required_version_above_common_version() {
        let error = parse_release_contract(
            r#"
[cargo]
manifest = "pishoo/Cargo.toml"

[build.env]
required = []

[destination.s3]
bucket = "download"
endpoint.env = "XTASK_RELEASE_S3_ENDPOINT_URL"
access_key_id.env = "XTASK_RELEASE_S3_ACCESS_KEY_ID"
secret_access_key.env = "XTASK_RELEASE_S3_SECRET_ACCESS_KEY"

[package.common]
name = "pishoo-common"
version = "0.5.1-1"
required_version = "0.5.2-1"
"#,
        )
        .expect_err("required version above published common version should fail");

        assert!(error.to_string().contains("required common version"));
    }

    #[test]
    fn rejects_old_top_level_common_contract() {
        let error = parse_release_contract(
            "common_version = \"0.5.2-1\"\nrequired_common_version = \"0.5.1-1\"\n",
        )
        .expect_err("old top-level contract must not parse");

        assert!(
            error
                .to_string()
                .contains("failed to parse release contract")
        );
    }

    #[test]
    fn rejects_missing_required_build_env() {
        let contract: ReleaseContract =
            parse_release_contract(CONTRACT).expect("contract should parse");
        let values = BTreeMap::new();

        let error = validate_required_build_env_values(&contract, &values)
            .expect_err("missing DHTTP_ROOT_CA should fail");

        assert_eq!(
            error.to_string(),
            "missing required build environment variable DHTTP_ROOT_CA"
        );
    }
}
