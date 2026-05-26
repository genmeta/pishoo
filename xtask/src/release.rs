pub mod apt;
pub mod artifact;
pub mod homebrew;
pub mod paths;
pub mod s3;
pub mod tap;
pub mod verify;

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
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

#[derive(Debug, Clone, Args)]
pub struct VerifyOptions {}

#[derive(Debug, Subcommand)]
pub enum PublishTarget {
    /// Publish staged artifacts to S3
    S3 {
        #[command(flatten)]
        options: S3Options,
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
    /// Upload selected staged roots: homebrew, apt
    #[arg(long = "root", value_enum, value_delimiter = ',')]
    pub roots: Vec<PublishRoot>,
    /// Remote prefix for APT repository files
    #[arg(long)]
    pub apt_prefix: Option<String>,
    /// Print planned uploads without writing to S3
    #[arg(long)]
    pub dry_run: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PublishRoot {
    /// target/common/homebrew
    Homebrew,
    /// target/common/apt
    Apt,
}

impl std::fmt::Display for PublishRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Homebrew => formatter.write_str("homebrew"),
            Self::Apt => formatter.write_str("apt"),
        }
    }
}

pub async fn stage(format: StageFormat) -> Result<(), Whatever> {
    match format {
        StageFormat::Homebrew => homebrew::stage().await,
        StageFormat::Apt { options } => apt::stage(options).await,
    }
}

pub async fn verify(options: VerifyOptions) -> Result<(), Whatever> {
    verify::run(options).await
}

pub async fn publish(target: PublishTarget) -> Result<(), Whatever> {
    match target {
        PublishTarget::S3 { options } => s3::publish(options).await,
        PublishTarget::Tap { options } => tap::publish(options).await,
    }
}

fn default_components() -> Vec<String> {
    vec!["main".to_owned()]
}
