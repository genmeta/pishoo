mod brew;
mod container;
mod deb;
mod release;
mod rpm;

use std::{ffi::OsString, io::IsTerminal, path::PathBuf, process::Stdio};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use snafu::{OptionExt, ResultExt, Whatever};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Build & packaging tasks for pishoo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Distribution packaging
    Dist {
        /// Grouped dist targets: deb/rpm/homebrew followed by target-local options
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        targets: Vec<OsString>,
    },
    /// Assemble publishable artifacts under target/common
    Stage {
        /// Grouped stage targets: homebrew/apt/rpm followed by target-local options
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        targets: Vec<OsString>,
    },
    /// Validate target/common before publishing
    Verify {
        #[command(subcommand)]
        command: release::VerifyCommand,
    },
    /// Publish staged artifacts
    Publish {
        #[command(subcommand)]
        target: release::PublishTarget,
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
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RpmTarget {
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

#[derive(Debug, Subcommand)]
enum DistFormat {
    /// Build .deb packages (via Docker container + cargo-zigbuild)
    Deb {
        /// Target triples (or "common" for arch-independent config package)
        #[arg(long = "target", required = true)]
        targets: Vec<DebTarget>,
        /// Build debug-profile binaries instead of release-profile binaries
        #[arg(long)]
        debug: bool,
        /// Cargo features to enable
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
        /// Sibling crate directories to bind-mount into the build container
        /// at `/{basename}`, matching `path = "../{basename}"` in Cargo.toml.
        /// Repeatable. Each path must exist and be a directory.
        #[arg(long = "sibling")]
        siblings: Vec<PathBuf>,
    },
    /// Build .rpm packages (via Fedora container + cargo-zigbuild + rpmbuild)
    Rpm {
        /// Target triples to build for
        #[arg(long = "target", required = true)]
        targets: Vec<RpmTarget>,
        /// Cargo features to enable
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
        /// Sibling crate directories to bind-mount into the build container.
        #[arg(long = "sibling")]
        siblings: Vec<PathBuf>,
    },
    /// Build Homebrew archives
    Homebrew {
        /// Target triples to build for
        #[arg(long = "target", required = true)]
        targets: Vec<BrewTarget>,
        /// Cargo features to enable
        #[arg(long = "features", value_delimiter = ',')]
        features: Vec<Feature>,
    },
}

#[derive(Parser)]
struct DistCli {
    #[command(subcommand)]
    format: DistFormat,
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::{
        BuildProfile, Cli, Command, parse_dist_format, parse_dist_sections,
        release::{PublishTarget, VerifyCommand},
    };

    fn subcommand<'a>(command: &'a clap::Command, name: &str) -> &'a clap::Command {
        command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == name)
            .expect("subcommand should be registered")
    }

    fn subcommand_names(command: &clap::Command) -> Vec<&str> {
        command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect()
    }

    fn argument_longs(command: &clap::Command) -> Vec<&str> {
        command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
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
    fn release_pipeline_subcommands_are_registered() {
        let command = Cli::command();
        let names = subcommand_names(&command);

        assert!(names.contains(&"stage"));
        assert!(names.contains(&"verify"));
        assert!(names.contains(&"publish"));
        assert!(!names.contains(&"tap"));
    }

    #[test]
    fn release_pipeline_uses_grouped_stage_targets() {
        let command = Cli::command();
        let stage = subcommand(&command, "stage");

        assert!(
            stage
                .clone()
                .render_long_help()
                .to_string()
                .contains("homebrew/apt/rpm")
        );
        assert!(stage.get_subcommands().next().is_none());
    }

    #[test]
    fn dist_accepts_grouped_targets_as_trailing_args() {
        let cli = Cli::try_parse_from([
            "xtask",
            "dist",
            "deb",
            "--target",
            "x86_64-unknown-linux-gnu",
            "rpm",
            "--target",
            "aarch64-unknown-linux-gnu",
            "homebrew",
            "--target",
            "x86_64-apple-darwin",
        ])
        .expect("grouped dist targets should parse at outer level");

        match cli.command {
            Command::Dist { targets } => {
                assert_eq!(
                    targets,
                    [
                        "deb",
                        "--target",
                        "x86_64-unknown-linux-gnu",
                        "rpm",
                        "--target",
                        "aarch64-unknown-linux-gnu",
                        "homebrew",
                        "--target",
                        "x86_64-apple-darwin",
                    ]
                    .map(std::ffi::OsString::from)
                );
            }
            _ => panic!("expected dist command"),
        }
    }

    #[test]
    fn dist_help_mentions_grouped_targets() {
        let help = subcommand(&Cli::command(), "dist")
            .clone()
            .render_long_help()
            .to_string();

        assert!(help.contains("Grouped dist targets: deb/rpm/homebrew"));
    }

    #[test]
    fn stage_accepts_grouped_targets_as_trailing_args() {
        let cli = Cli::try_parse_from([
            "xtask",
            "stage",
            "homebrew",
            "apt",
            "--suite",
            "stable",
            "--key-file",
            "key.asc",
            "--fingerprint",
            "00112233445566778899AABBCCDDEEFF00112233",
            "rpm",
        ])
        .expect("grouped stage targets should parse at outer level");

        match cli.command {
            Command::Stage { targets } => {
                assert_eq!(
                    targets,
                    [
                        "homebrew",
                        "apt",
                        "--suite",
                        "stable",
                        "--key-file",
                        "key.asc",
                        "--fingerprint",
                        "00112233445566778899AABBCCDDEEFF00112233",
                        "rpm",
                    ]
                    .map(std::ffi::OsString::from)
                );
            }
            _ => panic!("expected stage command"),
        }
    }

    #[test]
    fn stage_help_mentions_grouped_targets() {
        let help = subcommand(&Cli::command(), "stage")
            .clone()
            .render_long_help()
            .to_string();

        assert!(help.contains("Grouped stage targets: homebrew/apt/rpm"));
    }

    #[test]
    fn verify_local_accepts_grouped_targets_as_trailing_args() {
        let cli = Cli::try_parse_from(["xtask", "verify", "local", "homebrew", "rpm"])
            .expect("grouped verify local targets should parse at outer level");

        match cli.command {
            Command::Verify {
                command: VerifyCommand::Local { targets },
            } => {
                assert_eq!(targets, ["homebrew", "rpm"].map(std::ffi::OsString::from));
            }
            _ => panic!("expected verify local command"),
        }
    }

    #[test]
    fn verify_remote_s3_accepts_global_options_before_grouped_targets() {
        let cli = Cli::try_parse_from([
            "xtask",
            "verify",
            "remote",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
            "apt",
            "--prefix",
            "apt/stable",
            "rpm",
            "--prefix",
            "rpm/stable",
        ])
        .expect("grouped verify remote s3 targets should parse at outer level");

        match cli.command {
            Command::Verify {
                command:
                    VerifyCommand::Remote {
                        target: crate::release::RemoteVerifyTarget::S3 { options, targets },
                    },
            } => {
                assert_eq!(options.bucket, "downloads");
                assert_eq!(
                    targets,
                    [
                        "apt",
                        "--prefix",
                        "apt/stable",
                        "rpm",
                        "--prefix",
                        "rpm/stable",
                    ]
                    .map(std::ffi::OsString::from)
                );
            }
            _ => panic!("expected verify remote s3 command"),
        }
    }

    #[test]
    fn verify_remote_s3_requires_at_least_one_target() {
        let error = Cli::try_parse_from([
            "xtask",
            "verify",
            "remote",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
        ])
        .expect_err("verify remote s3 should require grouped targets");

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn verify_remote_s3_help_excludes_publish_only_options() {
        let command = Cli::command();
        let verify = subcommand(&command, "verify");
        let remote = subcommand(verify, "remote");
        let s3 = subcommand(remote, "s3");

        let help = s3.clone().render_long_help().to_string();

        assert!(help.contains("--endpoint-url"));
        assert!(help.contains("--bucket"));
        assert!(help.contains("--access-key-id-file"));
        assert!(help.contains("--secret-access-key-file"));
        assert!(!help.contains("--root"));
        assert!(!help.contains("--apt-prefix"));
        assert!(!help.contains("--dry-run"));
    }

    #[test]
    fn verify_remote_s3_rejects_legacy_root_option_with_grouped_target_message() {
        let error = match Cli::try_parse_from([
            "xtask",
            "verify",
            "remote",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
            "--root",
            "homebrew",
            "homebrew",
        ]) {
            Ok(_) => panic!("verify remote s3 should reject legacy --root"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(
            error
                .to_string()
                .contains("--root has been replaced by grouped s3 targets")
        );
    }

    #[test]
    fn verify_remote_s3_rejects_dry_run_as_publish_only() {
        let error = match Cli::try_parse_from([
            "xtask",
            "verify",
            "remote",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
            "--dry-run",
            "homebrew",
        ]) {
            Ok(_) => panic!("verify remote s3 should reject publish-only --dry-run"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(
            error
                .to_string()
                .contains("--dry-run is only supported by publish s3")
        );
    }

    #[test]
    fn dist_target_local_help_remains_clap_display_help() {
        let error = match parse_dist_format("deb", [std::ffi::OsString::from("--help")]) {
            Ok(_) => panic!("target-local help should be reported as clap display help"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("Build .deb packages"));
        assert!(error.to_string().contains("Usage: xtask dist deb"));
    }

    #[test]
    fn dist_target_local_parse_errors_use_clap_usage() {
        let error = match parse_dist_format("deb", [std::ffi::OsString::from("--bogus")]) {
            Ok(_) => panic!("invalid target-local options should be reported as clap errors"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        assert!(error.to_string().contains("Usage: xtask dist deb"));
        assert!(
            !error
                .to_string()
                .contains("failed to parse dist deb options")
        );
    }

    #[test]
    fn dist_sections_parse_later_help_before_execution() {
        let tokens = [
            std::ffi::OsString::from("deb"),
            std::ffi::OsString::from("--target"),
            std::ffi::OsString::from("x86_64-unknown-linux-gnu"),
            std::ffi::OsString::from("rpm"),
            std::ffi::OsString::from("--help"),
        ];

        let error = match parse_dist_sections(&tokens) {
            Ok(_) => panic!("later target-local help should stop grouped dist parsing"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("Build .rpm packages"));
        assert!(error.to_string().contains("Usage: xtask dist rpm"));
    }

    #[test]
    fn release_publish_uses_grouped_s3_targets_and_nested_tap_command_names() {
        let command = Cli::command();
        let publish = subcommand(&command, "publish");
        let publish_names = subcommand_names(publish);
        let s3_options = argument_longs(subcommand(publish, "s3"));

        assert!(publish_names.contains(&"s3"));
        assert!(publish_names.contains(&"tap"));
        assert!(!s3_options.contains(&"root"));
        assert!(!s3_options.contains(&"apt-prefix"));
    }

    #[test]
    fn publish_s3_help_mentions_grouped_targets_not_legacy_roots() {
        let command = Cli::command();
        let publish = subcommand(&command, "publish");
        let s3 = subcommand(publish, "s3");

        let help = s3.clone().render_long_help().to_string();

        assert!(help.contains("Grouped S3 targets: homebrew/apt/rpm"));
        assert!(!help.contains("--root"));
        assert!(!help.contains("--apt-prefix"));
    }

    #[test]
    fn publish_s3_requires_at_least_one_grouped_target() {
        let error = Cli::try_parse_from([
            "xtask",
            "publish",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
        ])
        .expect_err("publish s3 should require grouped targets");

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn publish_s3_rejects_legacy_root_option() {
        let error = match Cli::try_parse_from([
            "xtask",
            "publish",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
            "--root",
            "homebrew",
        ]) {
            Ok(_) => panic!("publish s3 should reject legacy --root"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(
            error
                .to_string()
                .contains("--root has been replaced by grouped s3 targets")
        );
    }

    #[test]
    fn publish_s3_accepts_grouped_rpm_target_with_prefix() {
        let cli = Cli::try_parse_from([
            "xtask",
            "publish",
            "s3",
            "--endpoint-url",
            "https://s3.example.test",
            "--bucket",
            "downloads",
            "--access-key-id-file",
            "access",
            "--secret-access-key-file",
            "secret",
            "rpm",
            "--prefix",
            "rpm/genmeta",
        ])
        .expect("publish s3 should accept grouped rpm target");

        match cli.command {
            Command::Publish {
                target: PublishTarget::S3 { options, targets },
            } => {
                assert_eq!(options.bucket, "downloads");
                assert_eq!(
                    targets,
                    ["rpm", "--prefix", "rpm/genmeta"].map(std::ffi::OsString::from)
                );
            }
            _ => panic!("expected publish s3 command"),
        }
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

fn parse_dist_format<I, T>(section_name: &str, args: I) -> Result<DistFormat, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut argv = vec![OsString::from("xtask dist"), section_name.to_owned().into()];
    argv.extend(args.into_iter().map(Into::into));
    DistCli::try_parse_from(argv).map(|cli| cli.format)
}

fn parse_dist_sections(tokens: &[OsString]) -> Result<Vec<DistFormat>, clap::Error> {
    let sections = release::grouped::parse_grouped_targets(tokens, &["deb", "rpm", "homebrew"])
        .map_err(|error| DistCli::command().error(ErrorKind::ValueValidation, error))?;

    sections
        .into_iter()
        .map(|section| parse_dist_format(&section.name, section.args))
        .collect()
}

async fn run_dist_sections(tokens: Vec<OsString>) -> Result<(), Whatever> {
    let formats = parse_dist_sections(&tokens).unwrap_or_else(|error| {
        error.exit();
    });

    for format in formats {
        match format {
            DistFormat::Deb {
                targets,
                debug,
                features,
                siblings,
            } => {
                deb::run(
                    &targets,
                    BuildProfile::from_debug(debug),
                    &features,
                    &siblings,
                )
                .await?
            }
            DistFormat::Rpm {
                targets,
                features,
                siblings,
            } => rpm::run(&targets, &features, &siblings).await?,
            DistFormat::Homebrew { targets, features } => brew::run(&targets, &features).await?,
        }
    }

    Ok(())
}

#[snafu::report]
#[tokio::main]
async fn main() -> Result<(), Whatever> {
    let _guard = init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Dist { targets } => run_dist_sections(targets).await?,
        Command::Stage { targets } => release::stage_sections(targets).await?,
        Command::Verify { command } => release::verify(command).await?,
        Command::Publish { target } => release::publish(target).await?,
    }
    Ok(())
}
