mod brew;
mod deb;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use snafu::{OptionExt, ResultExt, Whatever};

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

#[derive(Subcommand)]
enum DistFormat {
    /// Build .deb packages (via Docker container + cargo-zigbuild)
    Deb {
        /// Target triples (or "common" for arch-independent config package)
        #[arg(long = "target", required = true)]
        targets: Vec<String>,
    },
    /// Build Homebrew archives + formula
    Brew {
        /// Target triples to build for (e.g. aarch64-apple-darwin)
        #[arg(long = "target", required = true)]
        targets: Vec<String>,
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
fn sha256_file(path: &std::path::Path) -> Result<String, Whatever> {
    use sha2::Digest;
    let mut file =
        std::fs::File::open(path).whatever_context(format!("failed to open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .whatever_context(format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[snafu::report]
#[tokio::main]
async fn main() -> Result<(), Whatever> {
    let cli = Cli::parse();
    match cli.command {
        Command::Dist { format } => match format {
            DistFormat::Deb { targets } => deb::run(&targets).await?,
            DistFormat::Brew { targets } => brew::run(&targets)?,
        },
    }
    Ok(())
}
