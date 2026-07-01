use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use cargo_metadata::MetadataCommand;
use genmeta_xtask_release::{
    contract as shared_contract, package::PackageVersion, requires, system::PackageSystem,
};
use serde::Deserialize;
use snafu::{ResultExt, Snafu};

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
    #[serde(default)]
    pub target: BTreeMap<String, HomebrewTargetContract>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct HomebrewTargetContract {
    #[serde(default)]
    pub env: BTreeMap<String, BuildEnvBinding>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TemplateContract {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct BuildContract {
    #[serde(default)]
    pub env: BTreeMap<String, BuildEnvBinding>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BuildEnvBinding {
    pub env: Option<String>,
    pub value: Option<String>,
    #[serde(default)]
    pub optional: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Deb,
    Rpm,
    Brew,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ReleaseContractError {
    #[snafu(display("failed to read release contract"))]
    Read {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse shared release contract"))]
    Parse {
        source: toml::de::Error,
        path: PathBuf,
    },
    #[snafu(display("invalid shared release contract"))]
    SharedInvalid {
        source: shared_contract::ValidateContractError,
        path: PathBuf,
    },
    #[snafu(display("shared release contract must define product package with manifest"))]
    SharedMissingProduct { path: PathBuf },
    #[snafu(display("shared release contract product package must define deb branch"))]
    SharedMissingProductDeb { path: PathBuf },
    #[snafu(display(
        "shared release contract product package deb branch must require common package"
    ))]
    SharedMissingCommonDependency { path: PathBuf },
    #[snafu(display("shared release contract common package must define source version"))]
    SharedMissingCommonVersion { path: PathBuf },
    #[snafu(display("shared release contract common package must define deb branch"))]
    SharedMissingCommonDeb { path: PathBuf },
    #[snafu(display("failed to parse shared common package source version {version}"))]
    SharedCommonSourceVersion {
        source: semver::Error,
        path: PathBuf,
        version: String,
    },
    #[snafu(display("failed to resolve shared common package version"))]
    SharedCommonPackageVersion {
        source: genmeta_xtask_release::package::PackageVersionError,
        path: PathBuf,
    },
    #[snafu(display("failed to resolve shared package requirements"))]
    SharedRequirements {
        source: requires::ResolveRequiresError,
        path: PathBuf,
    },
    #[snafu(display(
        "shared release contract product package deb branch dependency missing minimum version"
    ))]
    SharedMissingCommonMinimum { path: PathBuf },
    #[snafu(display("shared release contract {system} branch missing template"))]
    SharedMissingTemplate {
        path: PathBuf,
        system: PackageSystem,
    },
    #[snafu(display("failed to read cargo metadata"))]
    CargoMetadata {
        source: cargo_metadata::Error,
        manifest: PathBuf,
    },
    #[snafu(display("cargo metadata did not return package for manifest"))]
    MissingPackageForManifest { manifest: PathBuf },
    #[snafu(display("cargo package is missing description"))]
    MissingDescription { manifest: PathBuf },
    #[snafu(display("cargo package is missing homepage"))]
    MissingHomepage { manifest: PathBuf },
    #[snafu(display("cargo package is missing license"))]
    MissingLicense { manifest: PathBuf },
    #[snafu(display("build env binding {name} must set exactly one of env or value"))]
    InvalidBuildEnvBinding { name: String },
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
    let contract = toml::from_str::<shared_contract::ReleaseContract>(input).context(
        release_contract_error::ParseSnafu {
            path: path.to_path_buf(),
        },
    )?;
    contract
        .validate()
        .context(release_contract_error::SharedInvalidSnafu {
            path: path.to_path_buf(),
        })?;
    release_contract_from_shared(path, contract)
}

fn release_contract_from_shared(
    path: &Path,
    contract: shared_contract::ReleaseContract,
) -> Result<ReleaseContract, ReleaseContractError> {
    let (product_id, product) = contract
        .package
        .iter()
        .find(|(_, package)| package.manifest.is_some())
        .ok_or_else(|| ReleaseContractError::SharedMissingProduct {
            path: path.to_path_buf(),
        })?;
    let product_deb =
        product
            .deb
            .as_ref()
            .ok_or_else(|| ReleaseContractError::SharedMissingProductDeb {
                path: path.to_path_buf(),
            })?;
    let (common_id, _) = product_deb.requires.iter().next().ok_or_else(|| {
        ReleaseContractError::SharedMissingCommonDependency {
            path: path.to_path_buf(),
        }
    })?;
    let common = contract.package.get(common_id).ok_or_else(|| {
        ReleaseContractError::SharedMissingCommonDependency {
            path: path.to_path_buf(),
        }
    })?;
    let common_version = common.version.as_deref().ok_or_else(|| {
        ReleaseContractError::SharedMissingCommonVersion {
            path: path.to_path_buf(),
        }
    })?;
    let common_deb =
        common
            .deb
            .as_ref()
            .ok_or_else(|| ReleaseContractError::SharedMissingCommonDeb {
                path: path.to_path_buf(),
            })?;
    let source_version = semver::Version::parse(common_version).context(
        release_contract_error::SharedCommonSourceVersionSnafu {
            path: path.to_path_buf(),
            version: common_version.to_string(),
        },
    )?;
    let common_package_version = PackageVersion::deb(source_version, common_deb.revision.clone())
        .context(release_contract_error::SharedCommonPackageVersionSnafu {
            path: path.to_path_buf(),
        })?
        .as_string();
    let required_common_version = requires::resolve_requires_for(
        &contract,
        &repo_root(),
        product_id.as_str(),
        PackageSystem::Deb,
    )
    .context(release_contract_error::SharedRequirementsSnafu {
        path: path.to_path_buf(),
    })?
    .remove(common_id.as_str())
    .and_then(|bounds| bounds.minimum)
    .ok_or_else(|| ReleaseContractError::SharedMissingCommonMinimum {
        path: path.to_path_buf(),
    })?;
    let manifest =
        product
            .manifest
            .clone()
            .ok_or_else(|| ReleaseContractError::SharedMissingProduct {
                path: path.to_path_buf(),
            })?;
    let homebrew = product
        .brew
        .as_ref()
        .map(|branch| shared_homebrew_contract(path, branch))
        .transpose()?;
    Ok(ReleaseContract {
        cargo: CargoSource { manifest },
        package: PackageContract {
            common: CommonPackageContract {
                name: common_id.as_str().to_string(),
                version: common_package_version.clone(),
                required_version: required_common_version,
            },
        },
        homebrew,
        build: BuildContract {
            env: shared_build_env(&product.build.env),
        },
        destination: shared_destination_contract(contract.destination),
    })
}

fn shared_homebrew_contract(
    path: &Path,
    branch: &shared_contract::BrewBranch,
) -> Result<HomebrewContract, ReleaseContractError> {
    let manifest_template = branch.manifest_template.clone().ok_or_else(|| {
        ReleaseContractError::SharedMissingTemplate {
            path: path.to_path_buf(),
            system: PackageSystem::Brew,
        }
    })?;
    Ok(HomebrewContract {
        template: TemplateContract {
            path: manifest_template,
        },
        target: branch
            .target
            .iter()
            .map(|(target, contract)| {
                (
                    target.clone(),
                    HomebrewTargetContract {
                        env: shared_build_env(&contract.env),
                    },
                )
            })
            .collect(),
    })
}

fn shared_build_env(
    env: &BTreeMap<String, shared_contract::EnvBinding>,
) -> BTreeMap<String, BuildEnvBinding> {
    env.iter()
        .map(|(name, binding)| {
            (
                name.clone(),
                BuildEnvBinding {
                    env: binding.env.clone(),
                    value: binding.value.clone(),
                    optional: binding.optional,
                },
            )
        })
        .collect()
}

fn shared_destination_contract(
    destination: shared_contract::DestinationContract,
) -> DestinationContract {
    let s3 = destination.s3;
    DestinationContract {
        s3: S3Destination {
            bucket: s3.bucket,
            endpoint: shared_env_ref(s3.endpoint),
            access_key_id: shared_env_ref(s3.access_key_id),
            secret_access_key: shared_env_ref(s3.secret_access_key),
        },
        brew: s3.brew.map(|branch| BrewDestination {
            prefix: branch.prefix,
            public_base_url: branch.public_base_url,
            tap: TapDestination {
                repository: branch.tap.repository,
                base_branch: branch.tap.base_branch,
                token: shared_env_ref(branch.tap.token),
            },
        }),
        deb: s3.deb.map(|branch| DebDestination {
            prefix: branch.prefix,
            suite: branch.suite,
            signing: DebSigning {
                key: shared_env_ref(branch.signing.key),
                passphrase: shared_env_ref(branch.signing.passphrase),
            },
            fingerprint: branch.fingerprint.map(shared_env_ref),
        }),
        rpm: s3.rpm.map(|branch| RpmDestination {
            prefix: branch.prefix,
        }),
    }
}

fn shared_env_ref(ref_: shared_contract::EnvRef) -> EnvRef {
    EnvRef { env: ref_.env }
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
    let package = metadata
        .root_package()
        .or_else(|| {
            metadata
                .packages
                .iter()
                .find(|package| package.manifest_path.as_std_path() == manifest)
        })
        .ok_or_else(|| ReleaseContractError::MissingPackageForManifest {
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

pub fn resolve_build_env_from_process(
    contract: &ReleaseContract,
    package_kind: PackageKind,
    target: Option<&str>,
) -> Result<BTreeMap<String, String>, ReleaseContractError> {
    let values = std::env::vars().collect::<BTreeMap<_, _>>();
    resolve_build_env_values(contract, package_kind, target, &values)
}

pub fn resolve_build_env_values(
    contract: &ReleaseContract,
    package_kind: PackageKind,
    target: Option<&str>,
    values: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ReleaseContractError> {
    let mut bindings = contract.build.env.clone();
    if package_kind == PackageKind::Brew
        && let (Some(homebrew), Some(target)) = (&contract.homebrew, target)
        && let Some(target_contract) = homebrew.target.get(target)
    {
        bindings.extend(target_contract.env.clone());
    }

    let mut resolved = BTreeMap::new();
    for (name, binding) in bindings {
        if let Some(value) = resolve_build_env_binding_value(&name, &binding, values)? {
            resolved.insert(name, value);
        }
    }
    Ok(resolved)
}

fn validate_build_env_binding(
    name: &str,
    binding: &BuildEnvBinding,
) -> Result<(), ReleaseContractError> {
    match (&binding.env, &binding.value) {
        (Some(_), None) | (None, Some(_)) => Ok(()),
        _ => Err(ReleaseContractError::InvalidBuildEnvBinding {
            name: name.to_owned(),
        }),
    }
}

fn resolve_build_env_binding_value(
    name: &str,
    binding: &BuildEnvBinding,
    values: &BTreeMap<String, String>,
) -> Result<Option<String>, ReleaseContractError> {
    validate_build_env_binding(name, binding)?;

    if let Some(env_name) = &binding.env {
        return resolve_env_ref(name, env_name, binding.optional, values);
    }

    let value = binding
        .value
        .as_ref()
        .expect("validated build env binding must have a value");
    if value.is_empty() {
        return Err(ReleaseContractError::EmptyBuildEnv {
            name: name.to_owned(),
        });
    }
    Ok(Some(value.clone()))
}

fn resolve_env_ref(
    _logical_name: &str,
    env_name: &str,
    optional: bool,
    values: &BTreeMap<String, String>,
) -> Result<Option<String>, ReleaseContractError> {
    let Some(value) = values.get(env_name) else {
        if optional {
            return Ok(None);
        }
        return Err(ReleaseContractError::MissingBuildEnv {
            name: env_name.to_owned(),
        });
    };

    if value.is_empty() {
        return Err(ReleaseContractError::EmptyBuildEnv {
            name: env_name.to_owned(),
        });
    }

    Ok(Some(value.clone()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        PackageKind, ReleaseContract, parse_release_contract, resolve_build_env_values,
        resolve_package_metadata,
    };

    const CONTRACT: &str = r#"
[package.pishoo]
manifest = "pishoo/Cargo.toml"

[package.pishoo.build.env.DHTTP_ROOT_CA]
env = "DHTTP_ROOT_CA"
container_path = "/dhttp-bootstrap/root.crt"

[package.pishoo.build.env.DHTTP_STUN_SERVER]
env = "DHTTP_STUN_SERVER"

[package.pishoo.build.env.DHTTP_H3_DNS_SERVER]
env = "DHTTP_H3_DNS_SERVER"

[package.pishoo.build.env.DHTTP_HTTP_DNS_SERVER]
env = "DHTTP_HTTP_DNS_SERVER"

[package.pishoo.build.env.DHTTP_MDNS_SERVICE]
env = "DHTTP_MDNS_SERVICE"

[package.pishoo.build.env.DHTTP_CERT_SERVER_URL]
env = "DHTTP_CERT_SERVER_URL"

[package.pishoo.build.env.DHTTP_GLOBAL_HOME]
env = "DHTTP_GLOBAL_HOME"
optional = true

[package.pishoo.deb]
revision = "1"
architecture = "target"
dockerfile = "xtask/release/deb/Dockerfile"

[package.pishoo.deb.requires.pishoo-common.version]
">=" = { from = "dependency" }
"<=" = { from = "self" }

[package.pishoo.rpm]
release = "1"
architecture = "target"
dockerfile = "xtask/release/rpm/Dockerfile"

[package.pishoo.rpm.requires.pishoo-common.version]
">=" = { from = "dependency" }
"<=" = { from = "self" }

[package.pishoo.brew]
script = "xtask/release/brew/pishoo.sh"
manifest_template = "xtask/templates/pishoo.rb.in"

[package.pishoo.brew.target.aarch64-apple-darwin.env.DHTTP_GLOBAL_HOME]
value = "/opt/homebrew/etc/dhttp"

[package.pishoo.brew.target.x86_64-apple-darwin.env.DHTTP_GLOBAL_HOME]
value = "/usr/local/etc/dhttp"

[package.pishoo-common]
version = "0.5.1"
description = "Common files for pishoo"
license = "Apache-2.0"
homepage = "https://dhttp.net"
repository = "https://github.com/genmeta/gateway"

[package.pishoo-common.deb]
revision = "1"
architecture = "all"
dockerfile = "xtask/release/deb/Dockerfile"

[package.pishoo-common.rpm]
release = "1"
architecture = "noarch"
dockerfile = "xtask/release/rpm/Dockerfile"

[destination.s3]
bucket = "download"
endpoint.env = "XTASK_RELEASE_S3_ENDPOINT_URL"
access_key_id.env = "XTASK_RELEASE_S3_ACCESS_KEY_ID"
secret_access_key.env = "XTASK_RELEASE_S3_SECRET_ACCESS_KEY"

[destination.s3.brew]
prefix = "homebrew"
public_base_url = "https://download.dhttp.net/homebrew"
tap.repository = "genmeta/homebrew-genmeta"
tap.base_branch = "main"
tap.token.env = "HOMEBREW_TAP_GITHUB_TOKEN"

[destination.s3.deb]
prefix = "ppa/genmeta"
suite = "genmeta"
signing.key.env = "XTASK_RELEASE_APT_SIGNING_KEY"
signing.passphrase.env = "XTASK_RELEASE_APT_SIGNING_PASSPHRASE"
fingerprint.env = "XTASK_RELEASE_APT_SIGNING_FINGERPRINT"

[destination.s3.rpm]
prefix = "rpm/pishoo"
"#;
    #[test]
    fn parses_release_contract() {
        let contract = parse_release_contract(CONTRACT).expect("contract should parse");

        assert_eq!(
            contract.cargo.manifest,
            std::path::Path::new("pishoo/Cargo.toml")
        );
        assert_eq!(contract.package.common.name, "pishoo-common");
        assert_eq!(contract.package.common.version, "0.5.1-1");
        assert_eq!(contract.package.common.required_version, "0.5.1-1");
        let brew = contract
            .destination
            .brew
            .as_ref()
            .expect("brew destination should parse");
        assert_eq!(brew.prefix, "homebrew");
        assert_eq!(brew.public_base_url, "https://download.dhttp.net/homebrew");
        let deb = contract
            .destination
            .deb
            .as_ref()
            .expect("deb destination should parse");
        assert_eq!(deb.prefix, "ppa/genmeta");
        assert_eq!(deb.suite, "genmeta");
        assert_eq!(
            contract.destination.rpm.as_ref().unwrap().prefix,
            "rpm/pishoo"
        );
    }

    #[test]
    fn committed_release_contract_uses_flat_product_layout() {
        let contract = super::load_release_contract().expect("committed contract should load");
        let brew = contract
            .destination
            .brew
            .as_ref()
            .expect("brew destination should exist");
        assert_eq!(brew.prefix, "homebrew");
        assert_eq!(brew.public_base_url, "https://download.dhttp.net/homebrew");
        let deb = contract
            .destination
            .deb
            .as_ref()
            .expect("deb destination should exist");
        assert_eq!(deb.prefix, "ppa/genmeta");
        assert_eq!(deb.suite, "genmeta");
    }

    #[test]
    fn resolves_metadata_for_workspace_member_manifest() {
        let contract = parse_release_contract(CONTRACT).expect("contract should parse");
        let metadata =
            resolve_package_metadata(&contract).expect("workspace member metadata should resolve");

        assert_eq!(metadata.name, "pishoo");
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
                .contains("failed to parse shared release contract")
        );
    }

    #[test]
    fn resolves_homebrew_target_override() {
        let contract: ReleaseContract =
            parse_release_contract(CONTRACT).expect("contract should parse");
        let values = BTreeMap::from([
            ("DHTTP_ROOT_CA".to_string(), "/tmp/root.crt".to_string()),
            (
                "DHTTP_STUN_SERVER".to_string(),
                "nat.genmeta.net:20004".to_string(),
            ),
            (
                "DHTTP_H3_DNS_SERVER".to_string(),
                "https://dns.genmeta.net:4433".to_string(),
            ),
            (
                "DHTTP_HTTP_DNS_SERVER".to_string(),
                "https://dns.genmeta.net".to_string(),
            ),
            ("DHTTP_MDNS_SERVICE".to_string(), "_dhttp.local".to_string()),
            (
                "DHTTP_CERT_SERVER_URL".to_string(),
                "https://license.genmeta.net".to_string(),
            ),
            (
                "DHTTP_GLOBAL_HOME".to_string(),
                "/runtime/should-be-overridden".to_string(),
            ),
        ]);

        let resolved = resolve_build_env_values(
            &contract,
            PackageKind::Brew,
            Some("x86_64-apple-darwin"),
            &values,
        )
        .expect("build env should resolve");

        assert_eq!(
            resolved.get("DHTTP_GLOBAL_HOME").map(String::as_str),
            Some("/usr/local/etc/dhttp")
        );
    }

    #[test]
    fn skips_missing_optional_global_home_env() {
        let contract: ReleaseContract =
            parse_release_contract(CONTRACT).expect("contract should parse");
        let values = BTreeMap::from([
            ("DHTTP_ROOT_CA".to_string(), "/tmp/root.crt".to_string()),
            (
                "DHTTP_STUN_SERVER".to_string(),
                "nat.genmeta.net:20004".to_string(),
            ),
            (
                "DHTTP_H3_DNS_SERVER".to_string(),
                "https://dns.genmeta.net:4433".to_string(),
            ),
            (
                "DHTTP_HTTP_DNS_SERVER".to_string(),
                "https://dns.genmeta.net".to_string(),
            ),
            ("DHTTP_MDNS_SERVICE".to_string(), "_dhttp.local".to_string()),
            (
                "DHTTP_CERT_SERVER_URL".to_string(),
                "https://license.genmeta.net".to_string(),
            ),
        ]);

        let resolved = resolve_build_env_values(&contract, PackageKind::Deb, None, &values)
            .expect("optional global home may be absent");

        assert!(!resolved.contains_key("DHTTP_GLOBAL_HOME"));
    }
}
