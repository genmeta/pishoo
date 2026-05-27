use std::ffi::OsString;

use clap::Args;
use snafu::Whatever;

#[derive(Debug, Clone, Args)]
pub struct S3Options {
    /// S3 endpoint URL
    #[arg(long)]
    pub endpoint_url: String,
    /// S3 bucket name
    #[arg(long)]
    pub bucket: String,
    /// AWS access key id
    #[arg(long, env = "XTASK_RELEASE_S3_ACCESS_KEY_ID", hide_env_values = true)]
    pub access_key_id: String,
    /// AWS secret access key
    #[arg(
        long,
        env = "XTASK_RELEASE_S3_SECRET_ACCESS_KEY",
        hide_env_values = true
    )]
    pub secret_access_key: String,
    /// Print the publish plan without uploading
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(_options: S3Options, _targets: Vec<OsString>) -> Result<(), Whatever> {
    snafu::whatever!("s3 publish execution is not wired yet")
}
