pub mod s3;

use clap::Subcommand;
use snafu::Whatever;

#[derive(Debug, Subcommand)]
pub enum PublishCommand {
    /// Publish package manifests to S3-compatible storage
    S3 {
        #[command(flatten)]
        options: s3::S3Options,
        /// Grouped publish targets: deb/rpm/brew followed by target-local options
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        targets: Vec<std::ffi::OsString>,
    },
}

pub async fn run(command: PublishCommand) -> Result<(), Whatever> {
    match command {
        PublishCommand::S3 { options, targets } => s3::run(options, targets).await,
    }
}
