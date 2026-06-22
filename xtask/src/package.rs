pub mod brew;
pub mod deb;
pub mod manifest;
pub mod prompt;
pub mod rpm;

use std::ffi::OsString;

use clap::{CommandFactory, Parser, Subcommand, error::ErrorKind};
#[allow(unused_imports)]
pub use manifest::{ArtifactKind, PackageArtifact, PackageManifest};
use snafu::{ResultExt, Whatever};

use crate::{BrewTarget, DebTarget, Feature, RpmTarget, grouped};

pub const KNOWN_PACKAGE_TARGETS: &[&str] = &["deb", "rpm", "brew"];

#[derive(Debug)]
pub struct PackageOptions {
    pub overwrite_manifest: bool,
    pub targets: Vec<OsString>,
}

#[derive(Debug, Subcommand)]
pub enum PackageFormat {
    Deb {
        #[arg(long = "target", required = true)]
        targets: Vec<DebTarget>,
        #[arg(long)]
        debug: bool,
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
        #[arg(long = "sibling")]
        siblings: Vec<std::path::PathBuf>,
    },
    Rpm {
        #[arg(long = "target", required = true)]
        targets: Vec<RpmTarget>,
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
        #[arg(long = "sibling")]
        siblings: Vec<std::path::PathBuf>,
    },
    Brew {
        #[arg(long = "target", required = true)]
        targets: Vec<BrewTarget>,
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
    },
}

#[derive(Debug, Parser)]
struct PackageCli {
    #[command(subcommand)]
    format: PackageFormat,
}

pub fn parse_package_format<I, T>(section_name: &str, args: I) -> Result<PackageFormat, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut argv = vec![
        OsString::from("xtask package"),
        section_name.to_owned().into(),
    ];
    argv.extend(args.into_iter().map(Into::into));
    PackageCli::try_parse_from(argv).map(|cli| cli.format)
}

pub fn parse_package_sections(tokens: &[OsString]) -> Result<Vec<PackageFormat>, clap::Error> {
    let sections = grouped::parse_grouped_targets(tokens, KNOWN_PACKAGE_TARGETS)
        .map_err(|error| PackageCli::command().error(ErrorKind::ValueValidation, error))?;
    sections
        .into_iter()
        .map(|section| parse_package_format(&section.name, section.args))
        .collect()
}

pub async fn run(options: PackageOptions) -> Result<(), Whatever> {
    let contract = crate::release_contract::load_release_contract()
        .whatever_context("failed to load release contract")?;
    let formats = parse_package_sections(&options.targets).unwrap_or_else(|error| error.exit());
    for format in formats {
        match format {
            PackageFormat::Deb {
                targets,
                debug,
                features,
                siblings,
            } => {
                deb::run(
                    &contract,
                    &targets,
                    crate::BuildProfile::from_debug(debug),
                    &features,
                    &siblings,
                    options.overwrite_manifest,
                )
                .await?
            }
            PackageFormat::Rpm {
                targets,
                features,
                siblings,
            } => {
                rpm::run(
                    &contract,
                    &targets,
                    &features,
                    &siblings,
                    options.overwrite_manifest,
                )
                .await?
            }
            PackageFormat::Brew { targets, features } => {
                brew::run(&contract, &targets, &features, options.overwrite_manifest).await?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PackageFormat, parse_package_format};
    use crate::RpmTarget;

    #[test]
    fn rpm_package_accepts_common_target() {
        let format = parse_package_format(
            "rpm",
            ["--target", "common", "--target", "x86_64-unknown-linux-gnu"],
        )
        .expect("rpm package format should parse");

        let PackageFormat::Rpm { targets, .. } = format else {
            panic!("expected rpm package format");
        };
        assert_eq!(targets, [RpmTarget::Common, RpmTarget::X86_64]);
    }
}
