pub mod artifact;
pub mod homebrew;
pub mod paths;
pub mod ppa;
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
    /// Stage Ubuntu PPA artifacts
    Ppa {
        #[command(flatten)]
        options: PpaOptions,
    },
    /// Stage all supported formats
    All {
        #[command(flatten)]
        options: PpaOptions,
    },
}

#[derive(Debug, Clone, Args)]
pub struct PpaOptions {
    /// APT suite name
    #[arg(long, default_value = "genmeta")]
    pub suite: String,
    /// Package components to stage
    #[arg(long = "component", default_values_t = default_components())]
    pub components: Vec<String>,
    /// ASCII-armored GPG private key file used to sign Release metadata
    #[arg(long)]
    pub key_file: PathBuf,
    /// Expected signing key fingerprint or long key id
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
    /// Upload only selected roots: homebrew, scoop, ppa
    #[arg(long, value_enum, value_delimiter = ',')]
    pub only: Vec<PublishRoot>,
    /// Remote prefix for APT repository files
    #[arg(long, default_value = "ppa/genmeta")]
    pub apt_prefix: String,
    /// Print planned uploads without writing to S3
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PublishRoot {
    /// target/common/homebrew
    Homebrew,
    /// target/common/scoop
    Scoop,
    /// target/common/ppa
    Ppa,
}

impl std::fmt::Display for PublishRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Homebrew => formatter.write_str("homebrew"),
            Self::Scoop => formatter.write_str("scoop"),
            Self::Ppa => formatter.write_str("ppa"),
        }
    }
}

pub async fn stage(format: StageFormat) -> Result<(), Whatever> {
    match format {
        StageFormat::Homebrew => homebrew::stage().await,
        StageFormat::Ppa { options } => ppa::stage(options).await,
        StageFormat::All { options } => {
            homebrew::stage().await?;
            ppa::stage(options).await
        }
    }
}

pub async fn verify(options: VerifyOptions) -> Result<(), Whatever> {
    verify::run(options).await
}

pub async fn publish(target: PublishTarget) -> Result<(), Whatever> {
    match target {
        PublishTarget::S3 { options } => s3::publish(options).await,
    }
}

pub async fn tap(repo: PathBuf, commit: bool, push: bool) -> Result<(), Whatever> {
    tap::update(repo, commit, push).await
}

fn default_components() -> Vec<String> {
    vec![
        "main".to_owned(),
        "contrib".to_owned(),
        "non-free".to_owned(),
    ]
}
