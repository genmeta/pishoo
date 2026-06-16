use std::path::{Path, PathBuf};

use serde::Deserialize;
use snafu::{ResultExt, Snafu};

use crate::version_cmp::{CompareVersionError, compare_deb_versions};

const RELEASE_CONTRACT_PATH: &str = "xtask/release.toml";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReleaseContract {
    pub common_version: String,
    pub required_common_version: String,
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
    #[snafu(display("failed to compare release contract versions"))]
    Compare { source: CompareVersionError },
    #[snafu(display(
        "required common version {required_common_version} exceeds common version {common_version}"
    ))]
    RequiredExceedsCommon {
        common_version: String,
        required_common_version: String,
    },
}

#[cfg(test)]
fn parse_release_contract(input: &str) -> Result<ReleaseContract, ReleaseContractError> {
    parse_release_contract_at(Path::new(RELEASE_CONTRACT_PATH), input)
}

pub fn load_release_contract() -> Result<ReleaseContract, ReleaseContractError> {
    let path = PathBuf::from(RELEASE_CONTRACT_PATH);
    let input = std::fs::read_to_string(&path)
        .context(release_contract_error::ReadSnafu { path: path.clone() })?;
    parse_release_contract_at(&path, &input)
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
    if compare_deb_versions(&contract.required_common_version, &contract.common_version)
        .context(release_contract_error::CompareSnafu)?
        .is_gt()
    {
        return Err(ReleaseContractError::RequiredExceedsCommon {
            common_version: contract.common_version.clone(),
            required_common_version: contract.required_common_version.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ReleaseContract, parse_release_contract};

    #[test]
    fn parses_release_contract() {
        let contract = parse_release_contract(
            "common_version = \"0.5.2-1\"\nrequired_common_version = \"0.5.1-1\"\n",
        )
        .expect("contract should parse");

        assert_eq!(
            contract,
            ReleaseContract {
                common_version: "0.5.2-1".to_string(),
                required_common_version: "0.5.1-1".to_string(),
            }
        );
    }

    #[test]
    fn rejects_required_version_above_common_version() {
        let error = parse_release_contract(
            "common_version = \"0.5.1-1\"\nrequired_common_version = \"0.5.2-1\"\n",
        )
        .expect_err("required version above published common version should fail");

        assert!(error.to_string().contains("required common version"));
    }
}
