pub mod apt;
pub mod artifact;
pub mod grouped;
pub mod homebrew;
pub mod paths;
pub mod rpm;
pub mod s3;
pub mod tap;
pub mod verify;

use std::{ffi::OsString, path::PathBuf};

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use snafu::Whatever;

#[derive(Debug, Subcommand)]
pub enum StageFormat {
    /// Stage Homebrew formulae and archives
    Homebrew,
    /// Stage APT repository packages and metadata
    Apt {
        #[command(flatten)]
        options: AptOptions,
    },
    /// Stage RPM packages
    Rpm,
}

#[derive(Debug, Parser)]
struct StageCli {
    #[command(subcommand)]
    format: StageFormat,
}

#[derive(Debug)]
pub enum StageSection {
    Homebrew,
    Apt(AptOptions),
    Rpm,
}

#[derive(Debug, Clone, Args)]
pub struct AptOptions {
    /// APT suite name
    #[arg(long)]
    pub suite: String,
    /// Package components to stage
    #[arg(long = "component", default_values_t = default_components())]
    pub components: Vec<String>,
    /// ASCII-armored GPG private key file used to sign Release metadata
    #[arg(long)]
    pub key_file: PathBuf,
    /// Expected full signing key fingerprint
    #[arg(long)]
    pub fingerprint: String,
    /// Optional file containing the GPG key passphrase
    #[arg(long)]
    pub passphrase_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum VerifyCommand {
    /// Verify selected staged roots on local disk
    Local {
        /// Grouped verify targets: homebrew/apt/rpm
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        targets: Vec<OsString>,
    },
    /// Verify selected staged roots against a remote destination
    Remote {
        #[command(subcommand)]
        target: RemoteVerifyTarget,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteVerifyTarget {
    /// Verify immutable artifact collisions in S3
    S3 {
        #[command(flatten)]
        options: S3VerifyOptions,
        /// Grouped S3 targets: homebrew/apt/rpm followed by target-local options
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_parser = verify_s3_target_token
        )]
        targets: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
pub enum PublishTarget {
    /// Publish staged artifacts to S3
    S3 {
        #[command(flatten)]
        options: S3Options,
        /// Grouped S3 targets: homebrew/apt/rpm followed by target-local options
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_parser = publish_s3_target_token
        )]
        targets: Vec<OsString>,
    },
    /// Publish Homebrew formulae to a tap checkout
    Tap {
        #[command(flatten)]
        options: TapOptions,
    },
}

#[derive(Debug, Clone, Args)]
pub struct S3Options {
    /// S3 endpoint URL
    #[arg(long)]
    pub endpoint_url: String,
    /// S3 bucket name
    #[arg(long)]
    pub bucket: String,
    /// File containing AWS access key id
    #[arg(long)]
    pub access_key_id_file: PathBuf,
    /// File containing AWS secret access key
    #[arg(long)]
    pub secret_access_key_file: PathBuf,
    /// Print planned uploads without writing to S3
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Args)]
pub struct S3VerifyOptions {
    /// S3 endpoint URL
    #[arg(long)]
    pub endpoint_url: String,
    /// S3 bucket name
    #[arg(long)]
    pub bucket: String,
    /// File containing AWS access key id
    #[arg(long)]
    pub access_key_id_file: PathBuf,
    /// File containing AWS secret access key
    #[arg(long)]
    pub secret_access_key_file: PathBuf,
}

fn verify_s3_target_token(value: &str) -> Result<OsString, String> {
    if value == "--dry-run" {
        return Err(format!(
            "{value} is only supported by publish s3, not verify remote s3"
        ));
    }
    if is_legacy_s3_publish_option(value) {
        return Err(legacy_s3_publish_option_error(value));
    }
    Ok(value.into())
}

fn publish_s3_target_token(value: &str) -> Result<OsString, String> {
    if is_legacy_s3_publish_option(value) {
        return Err(legacy_s3_publish_option_error(value));
    }
    Ok(value.into())
}

fn is_legacy_s3_publish_option(value: &str) -> bool {
    matches!(value, "--root" | "--apt-prefix")
        || value.starts_with("--root=")
        || value.starts_with("--apt-prefix=")
}

fn legacy_s3_publish_option_error(value: &str) -> String {
    format!(
        "{value} has been replaced by grouped s3 targets; use target-local apt --prefix or rpm --prefix for repository prefixes"
    )
}

#[derive(Debug, Clone, Args)]
pub struct TapOptions {
    /// Homebrew tap repository checkout path
    pub repo: PathBuf,
    /// Commit changes after copying formulae
    #[arg(long)]
    pub commit: bool,
    /// Push the tap repository after committing
    #[arg(long)]
    pub push: bool,
    /// Print planned tap updates without mutating the repository
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_stage_format<I, T>(section_name: &str, args: I) -> Result<StageSection, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut argv = vec![
        OsString::from("xtask stage"),
        section_name.to_owned().into(),
    ];
    argv.extend(args.into_iter().map(Into::into));
    StageCli::try_parse_from(argv)
        .map(|cli| match cli.format {
            StageFormat::Homebrew => StageSection::Homebrew,
            StageFormat::Rpm => StageSection::Rpm,
            StageFormat::Apt { options } => StageSection::Apt(options),
        })
        .and_then(|section| validate_stage_section(section_name, section))
}

fn validate_stage_section(
    section_name: &str,
    section: StageSection,
) -> Result<StageSection, clap::Error> {
    if let StageSection::Apt(options) = &section {
        validate_apt_options(options).map_err(|error| {
            stage_section_error(section_name, ErrorKind::ValueValidation, error)
        })?;
    }
    Ok(section)
}

fn validate_apt_options(options: &AptOptions) -> Result<(), Whatever> {
    validate_apt_path_segment("suite", &options.suite)?;
    snafu::ensure_whatever!(
        !options.components.is_empty(),
        "at least one apt component is required"
    );
    for component in &options.components {
        validate_apt_path_segment("component", component)?;
    }
    Ok(())
}

fn validate_apt_path_segment(kind: &str, value: &str) -> Result<(), Whatever> {
    snafu::ensure_whatever!(!value.is_empty(), "{kind} must not be empty");
    snafu::ensure_whatever!(
        !value.contains('/') && !value.contains('\\'),
        "{kind} must be a single path segment"
    );
    snafu::ensure_whatever!(
        value != "." && value != "..",
        "{kind} must be a normal path segment"
    );
    Ok(())
}

fn stage_section_error(
    section_name: &str,
    kind: ErrorKind,
    message: impl std::fmt::Display,
) -> clap::Error {
    let mut command = StageCli::command().bin_name("xtask stage");
    command.build();
    match command.find_subcommand_mut(section_name) {
        Some(subcommand) => subcommand.error(kind, message),
        None => command.error(kind, message),
    }
}

pub fn parse_stage_sections(tokens: &[OsString]) -> Result<Vec<StageSection>, clap::Error> {
    let sections = grouped::parse_grouped_targets(tokens, &["homebrew", "apt", "rpm"])
        .map_err(|error| stage_error(ErrorKind::ValueValidation, error))?;

    sections
        .into_iter()
        .map(|section| parse_stage_format(&section.name, section.args))
        .collect()
}

fn stage_error(kind: ErrorKind, message: impl std::fmt::Display) -> clap::Error {
    StageCli::command()
        .bin_name("xtask stage")
        .error(kind, message)
}

pub async fn stage_sections(tokens: Vec<OsString>) -> Result<(), Whatever> {
    let sections = parse_stage_sections(&tokens).unwrap_or_else(|error| {
        error.exit();
    });

    for section in sections {
        match section {
            StageSection::Homebrew => homebrew::stage().await?,
            StageSection::Apt(options) => apt::stage(options).await?,
            StageSection::Rpm => rpm::stage().await?,
        }
    }

    Ok(())
}

pub async fn verify(command: VerifyCommand) -> Result<(), Whatever> {
    match command {
        VerifyCommand::Local { targets } => {
            let roots = verify::parse_local_targets(&targets).unwrap_or_else(|error| {
                error.exit();
            });
            verify::run_local(&roots).await
        }
        VerifyCommand::Remote { target } => match target {
            RemoteVerifyTarget::S3 { options, targets } => {
                s3::verify_remote(options, targets).await
            }
        },
    }
}

pub async fn publish(target: PublishTarget) -> Result<(), Whatever> {
    match target {
        PublishTarget::S3 { options, targets } => s3::publish(options, targets).await,
        PublishTarget::Tap { options } => tap::publish(options).await,
    }
}

fn default_components() -> Vec<String> {
    vec!["main".to_owned()]
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::error::ErrorKind;

    use super::{StageSection, parse_stage_sections};

    fn os(value: &str) -> OsString {
        OsString::from(value)
    }

    #[test]
    fn stage_sections_parse_later_apt_error_before_execution() {
        let tokens = [os("homebrew"), os("apt"), os("--bogus"), os("rpm")];

        let error = match parse_stage_sections(&tokens) {
            Ok(_) => panic!("later target-local parse error should stop grouped stage parsing"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(error.to_string().contains("Usage: xtask stage apt"));
    }

    #[test]
    fn stage_sections_parse_later_apt_help_before_execution() {
        let tokens = [os("homebrew"), os("apt"), os("--help"), os("rpm")];

        let error = match parse_stage_sections(&tokens) {
            Ok(_) => panic!("later target-local help should stop grouped stage parsing"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("Stage APT repository"));
        assert!(error.to_string().contains("Usage: xtask stage apt"));
    }

    #[test]
    fn stage_sections_reject_invalid_later_apt_options_before_execution() {
        let tokens = [
            os("homebrew"),
            os("apt"),
            os("--suite"),
            os("../bad"),
            os("--key-file"),
            os("key.asc"),
            os("--fingerprint"),
            os("00112233445566778899AABBCCDDEEFF00112233"),
        ];

        let error = match parse_stage_sections(&tokens) {
            Ok(_) => panic!("later apt semantic validation should stop grouped stage parsing"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(
            error
                .to_string()
                .contains("suite must be a single path segment")
        );
        assert!(error.to_string().contains("Usage: xtask stage apt"));
    }

    #[test]
    fn stage_sections_unknown_target_error_uses_stage_usage() {
        let tokens = [os("unknown")];

        let error = parse_stage_sections(&tokens).expect_err("unknown target should fail");
        let text = error.to_string();

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(text.contains("unknown target unknown"));
        assert!(text.contains("Usage: xtask stage"));
        assert!(!text.contains("Usage: xtask <COMMAND>"));
    }

    #[test]
    fn stage_sections_argument_before_target_error_uses_stage_usage() {
        let tokens = [os("--suite"), os("stable"), os("apt")];

        let error = parse_stage_sections(&tokens).expect_err("argument before target should fail");
        let text = error.to_string();

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(text.contains("expected a target before argument --suite"));
        assert!(text.contains("Usage: xtask stage"));
        assert!(!text.contains("Usage: xtask <COMMAND>"));
    }

    #[test]
    fn stage_sections_reject_no_option_target_arguments() {
        let tokens = [os("rpm"), os("--prefix"), os("download")];

        let error = match parse_stage_sections(&tokens) {
            Ok(_) => panic!("rpm does not accept target-local options"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(error.to_string().contains("Usage: xtask stage rpm"));
    }

    #[test]
    fn stage_sections_preserve_user_order() {
        let tokens = [
            os("rpm"),
            os("homebrew"),
            os("apt"),
            os("--suite"),
            os("stable"),
            os("--key-file"),
            os("key.asc"),
            os("--fingerprint"),
            os("00112233445566778899AABBCCDDEEFF00112233"),
        ];

        let sections = parse_stage_sections(&tokens).expect("stage sections should parse");

        assert!(matches!(sections[0], StageSection::Rpm));
        assert!(matches!(sections[1], StageSection::Homebrew));
        assert!(matches!(sections[2], StageSection::Apt(_)));
    }
}
