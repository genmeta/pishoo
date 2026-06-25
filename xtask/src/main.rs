mod brew;
mod container;
mod deb;
mod grouped;
mod package;
mod publish;
mod release_contract;
mod rpm;
mod template;
mod version_cmp;

use std::{ffi::OsString, io::IsTerminal, path::PathBuf, process::Stdio};

use clap::{Parser, Subcommand, ValueEnum};
use snafu::{OptionExt, ResultExt, Whatever};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Build & packaging tasks for pishoo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build package artifacts and write package manifests
    Package {
        /// Allow replacing an existing target/common/<kind>/manifest.toml
        #[arg(long)]
        overwrite_manifest: bool,
        /// Grouped package targets: deb/rpm/brew followed by target-local options
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_parser = package_token
        )]
        targets: Vec<OsString>,
    },
    /// Publish package manifests
    Publish {
        #[command(subcommand)]
        command: publish::PublishCommand,
    },
}

fn package_token(value: &str) -> Result<OsString, String> {
    if value == "scoop" || value == "homebrew" {
        return Err("unknown package target".to_string());
    }
    Ok(OsString::from(value))
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
    /// i686-unknown-linux-gnu
    #[value(name = "i686-unknown-linux-gnu")]
    I686,
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
            Self::I686 => "i686-unknown-linux-gnu",
            Self::Common => "common",
        }
    }
}

/// Supported target triples for .rpm builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RpmTarget {
    /// Arch-independent pishoo-common config package
    #[value(name = "common")]
    Common,
    /// x86_64-unknown-linux-gnu -> x86_64
    #[value(name = "x86_64-unknown-linux-gnu")]
    X86_64,
    /// aarch64-unknown-linux-gnu -> aarch64
    #[value(name = "aarch64-unknown-linux-gnu")]
    Aarch64,
    /// armv7-unknown-linux-gnueabihf -> armv7hl
    #[value(name = "armv7-unknown-linux-gnueabihf")]
    Armv7,
    /// i686-unknown-linux-gnu -> i686
    #[value(name = "i686-unknown-linux-gnu")]
    I686,
}

impl RpmTarget {
    pub fn triple(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::X86_64 => "x86_64-unknown-linux-gnu",
            Self::Aarch64 => "aarch64-unknown-linux-gnu",
            Self::Armv7 => "armv7-unknown-linux-gnueabihf",
            Self::I686 => "i686-unknown-linux-gnu",
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
    /// Enable SSH daemon support (pishoo-ssh-session)
    Sshd,
    /// Enable PAM authentication (implies sshd)
    Pam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Release,
    Debug,
}

impl BuildProfile {
    fn from_debug(debug: bool) -> Self {
        if debug { Self::Debug } else { Self::Release }
    }

    pub fn cargo_profile_args(self) -> Vec<&'static str> {
        match self {
            Self::Release => vec!["--release"],
            Self::Debug => Vec::new(),
        }
    }

    pub fn target_dir_name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Debug => "debug",
        }
    }
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
    })
}

/// Compute SHA-256 hex digest of a file.
async fn sha256_file(path: &std::path::Path) -> Result<String, Whatever> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;

        use sha2::Digest;

        let mut file = std::fs::File::open(&path)
            .whatever_context(format!("failed to open {}", path.display()))?;
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0; 8192];
        loop {
            let read = file
                .read(&mut buffer)
                .whatever_context(format!("failed to read {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex_lower(hasher.finalize().as_ref()))
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::{BuildProfile, Cli, Command};

    const RELEASE_WORKFLOW: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.github/workflows/release.yml"
    ));

    fn subcommand_names(command: &clap::Command) -> Vec<&str> {
        command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect()
    }

    #[test]
    fn release_profile_uses_release_cargo_flag_and_dir() {
        assert_eq!(BuildProfile::Release.cargo_profile_args(), ["--release"]);
        assert_eq!(BuildProfile::Release.target_dir_name(), "release");
    }

    #[test]
    fn debug_profile_omits_release_cargo_flag_and_uses_debug_dir() {
        assert!(BuildProfile::Debug.cargo_profile_args().is_empty());
        assert_eq!(BuildProfile::Debug.target_dir_name(), "debug");
    }

    #[test]
    fn release_pipeline_subcommands_are_package_and_publish() {
        let command = Cli::command();
        let names = subcommand_names(&command);

        assert!(names.contains(&"package"));
        assert!(names.contains(&"publish"));
        assert!(!names.contains(&"dist"));
        assert!(!names.contains(&"stage"));
        assert!(!names.contains(&"verify"));
    }

    #[test]
    fn gateway_package_accepts_features_and_common_deb() {
        let cli = Cli::try_parse_from([
            "xtask",
            "package",
            "--overwrite-manifest",
            "deb",
            "--target",
            "common",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--features",
            "sshd,pam",
            "brew",
            "--target",
            "aarch64-apple-darwin",
            "--features",
            "sshd,pam",
        ])
        .expect("gateway package should parse");

        match cli.command {
            Command::Package {
                overwrite_manifest,
                targets,
            } => {
                assert!(overwrite_manifest);
                assert_eq!(targets[0], std::ffi::OsString::from("deb"));
            }
            _ => panic!("expected package command"),
        }
    }

    #[test]
    fn gateway_rejects_scoop_target() {
        let error = Cli::try_parse_from(["xtask", "package", "scoop"])
            .expect_err("gateway should not support scoop");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn publish_s3_accepts_grouped_targets() {
        let cli = Cli::try_parse_from(["xtask", "publish", "s3", "--dry-run", "deb", "brew"])
            .expect("publish command should parse");

        match cli.command {
            Command::Publish { command } => match command {
                crate::publish::PublishCommand::S3 { options, targets } => {
                    assert!(options.dry_run);
                    assert_eq!(targets[0], std::ffi::OsString::from("deb"));
                    assert_eq!(targets[1], std::ffi::OsString::from("brew"));
                }
            },
            _ => panic!("expected publish command"),
        }
    }

    #[test]
    fn old_release_commands_are_rejected() {
        for command in ["dist", "stage", "verify"] {
            let error = Cli::try_parse_from(["xtask", command])
                .expect_err("old release command should be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        }
    }

    #[test]
    fn release_workflow_publish_commands_are_tag_mode_safe() {
        assert!(!RELEASE_WORKFLOW.contains("publish_args=()"));
        assert!(!RELEASE_WORKFLOW.contains("\"${publish_args[@]}\""));
        assert!(RELEASE_WORKFLOW.contains("DHTTP_ROOT_CA_PEM: ${{ secrets.DHTTP_ROOT_CA_PEM }}"));
        assert!(
            RELEASE_WORKFLOW
                .contains("DHTTP_ROOT_CA: ${{ github.workspace }}/.release/dhttp-root-ca.pem")
        );
        assert!(RELEASE_WORKFLOW.contains("Materialize DHTTP root CA"));
        assert!(
            RELEASE_WORKFLOW.contains("missing required release configuration: DHTTP_ROOT_CA_PEM")
        );
        assert!(!RELEASE_WORKFLOW.contains("keychain/root.crt"));
        assert!(!RELEASE_WORKFLOW.contains("--endpoint-url"));
        assert!(!RELEASE_WORKFLOW.contains("--bucket"));
        assert!(!RELEASE_WORKFLOW.contains("--prefix"));
        assert!(!RELEASE_WORKFLOW.contains("--public-base-url"));
        assert!(RELEASE_WORKFLOW.contains("\"${publish_cmd[@]}\" deb"));
        assert!(RELEASE_WORKFLOW.contains("\"${publish_cmd[@]}\" rpm"));
        assert!(RELEASE_WORKFLOW.contains("\"${publish_cmd[@]}\" brew"));
        assert_eq!(
            RELEASE_WORKFLOW
                .matches("publish_cmd=(cargo xtask publish s3)")
                .count(),
            3
        );
    }

    #[test]
    fn release_workflow_uploads_product_assets_to_github_release() {
        assert!(RELEASE_WORKFLOW.contains("contents: write"));
        assert!(RELEASE_WORKFLOW.contains("  github-release:"));
        assert_eq!(RELEASE_WORKFLOW.matches("needs: github-release").count(), 3);
        assert_eq!(
            RELEASE_WORKFLOW
                .matches("gh release upload \"$GITHUB_REF_NAME\"")
                .count(),
            3
        );
        assert_eq!(
            RELEASE_WORKFLOW
                .matches("gh release create \"$GITHUB_REF_NAME\"")
                .count(),
            1
        );
        assert!(RELEASE_WORKFLOW.contains("git for-each-ref \"$tag_ref\" --format='%(contents)'"));
        assert!(RELEASE_WORKFLOW.contains("## Authentication and provenance"));
        assert!(RELEASE_WORKFLOW.contains("--notes-file \"$notes_file\""));
        assert!(!RELEASE_WORKFLOW.contains("--notes-from-tag               ||"));
        assert!(
            RELEASE_WORKFLOW
                .contains("assets=(target/common/deb/*.deb target/*/release/deb/*.deb)")
        );
        assert!(
            RELEASE_WORKFLOW
                .contains("assets=(target/common/rpm/*.rpm target/*/release/rpm/*.rpm)")
        );
        assert!(
            RELEASE_WORKFLOW
                .contains("assets=(target/*/release/brew/*.tar.gz target/common/brew/*.rb)")
        );
    }

    #[test]
    fn release_workflow_homebrew_tap_updates_root_formula() {
        assert!(RELEASE_WORKFLOW.contains("id: homebrew_destination"));
        assert!(!RELEASE_WORKFLOW.contains("download.genmeta.net"));
        assert!(RELEASE_WORKFLOW.contains("tomllib.loads(Path(\"xtask/release.toml\")"));
        assert!(RELEASE_WORKFLOW.contains("formula_dest=\"$tap_dir/$FORMULA_NAME\""));
        assert!(RELEASE_WORKFLOW.contains("git status --porcelain -- \"$FORMULA_NAME\""));
        assert!(RELEASE_WORKFLOW.contains("git add \"$FORMULA_NAME\""));
        assert!(!RELEASE_WORKFLOW.contains("Formula/$FORMULA_NAME"));
    }

    #[test]
    fn public_package_manifests_declare_apache_2_license() {
        let workspace_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask manifest should be under workspace");
        for manifest in ["gateway/Cargo.toml", "pishoo/Cargo.toml"] {
            let manifest_path = workspace_dir.join(manifest);
            let contents = std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
            assert!(
                contents.contains("license = \"Apache-2.0\""),
                "{manifest} should declare Apache-2.0"
            );
        }
    }

    #[test]
    fn debian_package_metadata_declares_apache_2_license() {
        let copyright_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("deb")
            .join("copyright");
        let contents = std::fs::read_to_string(&copyright_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", copyright_path.display()));
        assert!(contents.contains("License: Apache-2.0"));
    }
}

/// Run an external command quietly, suppressing stdout/stderr.
pub async fn run_cmd_quiet(cmd: &mut tokio::process::Command) -> Result<(), Whatever> {
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
        Command::Package {
            overwrite_manifest,
            targets,
        } => {
            package::run(package::PackageOptions {
                overwrite_manifest,
                targets,
            })
            .await?
        }
        Command::Publish { command } => publish::run(command).await?,
    }
    Ok(())
}
