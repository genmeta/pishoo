mod brew;
mod deb;

use std::{io::IsTerminal, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use snafu::{OptionExt, ResultExt, Whatever};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "xtask", about = "Build & packaging tasks for pishoo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Distribution packaging
    Dist {
        #[command(subcommand)]
        format: DistFormat,
    },
}

/// Supported target triples for .deb builds.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DebTarget {
    /// x86_64-unknown-linux-gnu
    #[value(name = "x86_64-unknown-linux-gnu")]
    X86_64,
    /// aarch64-unknown-linux-gnu
    #[value(name = "aarch64-unknown-linux-gnu")]
    Aarch64,
    /// armv7-unknown-linux-gnueabihf
    #[value(name = "armv7-unknown-linux-gnueabihf")]
    Armv7,
    /// Arch-independent pishoo-common config package
    #[value(name = "common")]
    Common,
}

impl DebTarget {
    pub fn triple(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-gnu",
            Self::Aarch64 => "aarch64-unknown-linux-gnu",
            Self::Armv7 => "armv7-unknown-linux-gnueabihf",
            Self::Common => "common",
        }
    }
}

/// Supported target triples for Homebrew builds.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BrewTarget {
    /// aarch64-apple-darwin
    #[value(name = "aarch64-apple-darwin")]
    Aarch64,
    /// x86_64-apple-darwin
    #[value(name = "x86_64-apple-darwin")]
    X86_64,
}

impl BrewTarget {
    pub fn triple(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-apple-darwin",
            Self::X86_64 => "x86_64-apple-darwin",
        }
    }
}

/// Cargo features to enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Feature {
    /// Enable SSH daemon support (pishoo-worker, pishoo-ssh-session)
    Sshd,
    /// Enable PAM authentication (implies sshd)
    Pam,
}

#[derive(Subcommand)]
enum DistFormat {
    /// Build .deb packages (via Docker container + cargo-zigbuild)
    Deb {
        /// Target triples (or "common" for arch-independent config package)
        #[arg(long = "target", required = true)]
        targets: Vec<DebTarget>,
        /// Cargo features to enable
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
    },
    /// Build Homebrew archives + formula
    Brew {
        /// Target triples to build for
        #[arg(long = "target", required = true)]
        targets: Vec<BrewTarget>,
    },
}

/// Resolve the workspace target directory via cargo_metadata.
fn target_dir() -> Result<PathBuf, Whatever> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .whatever_context("failed to read cargo metadata")?;
    Ok(metadata.target_directory.into_std_path_buf())
}

/// Package version from cargo_metadata.
fn package_version(name: &str) -> Result<String, Whatever> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .whatever_context("failed to read cargo metadata")?;
    let pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == name)
        .whatever_context(format!("package {name} not found in workspace"))?;
    Ok(pkg.version.to_string())
}

/// Package metadata (version, description, homepage, license).
struct PackageMeta {
    version: String,
    description: String,
    homepage: String,
    license: String,
}

fn package_meta(name: &str) -> Result<PackageMeta, Whatever> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .whatever_context("failed to read cargo metadata")?;
    let pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == name)
        .whatever_context(format!("package {name} not found in workspace"))?;
    Ok(PackageMeta {
        version: pkg.version.to_string(),
        description: pkg.description.clone().unwrap_or_default(),
        homepage: pkg.homepage.clone().unwrap_or_default(),
        license: pkg.license.clone().unwrap_or_default(),
    })
}

/// Compute SHA-256 hex digest of a file.
async fn sha256_file(path: &std::path::Path) -> Result<String, Whatever> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use sha2::Digest;
        let mut file = std::fs::File::open(&path)
            .whatever_context(format!("failed to open {}", path.display()))?;
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut file, &mut hasher)
            .whatever_context(format!("failed to read {}", path.display()))?;
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .whatever_context("sha256 task panicked")?
}

/// Run an external command, checking its exit status.
pub async fn run_cmd(cmd: &mut tokio::process::Command) -> Result<(), Whatever> {
    let status = cmd
        .status()
        .await
        .whatever_context("failed to spawn process")?;
    snafu::ensure_whatever!(status.success(), "command exited with {status}");
    Ok(())
}

fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    let (stderr, guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(stderr),
        )
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    guard
}

#[snafu::report]
#[tokio::main]
async fn main() -> Result<(), Whatever> {
    let _guard = init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Dist { format } => match format {
            DistFormat::Deb { targets, features } => {
                deb::run(&targets, &features).await?;
            }
            DistFormat::Brew { targets } => brew::run(&targets).await?,
        },
    }
    Ok(())
}
