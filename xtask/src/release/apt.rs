use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use flate2::{Compression, write::GzEncoder};
use snafu::{OptionExt, ResultExt, Whatever};
use tempfile::TempDir;
use tracing::info;

use super::{
    AptOptions,
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, copy_artifact, read_manifest, relative_path,
        sha256_file, write_manifest,
    },
    paths::{common_paths, promote_staged_outputs, recreate_dir},
};
use crate::{run_cmd, target_dir};

const PACKAGE_NAME: &str = "pishoo";
const DEB_SEARCH_DIRS: [&str; 5] = [
    "x86_64-unknown-linux-gnu/release/deb",
    "aarch64-unknown-linux-gnu/release/deb",
    "armv7-unknown-linux-gnueabihf/release/deb",
    "i686-unknown-linux-gnu/release/deb",
    "common/deb",
];
const APT_ARCHES: [&str; 4] = ["amd64", "arm64", "armhf", "i386"];

#[derive(Debug)]
struct DebSource {
    package: String,
    version: String,
    filename: String,
    source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryMetadataPaths {
    packages: PathBuf,
    packages_gz: PathBuf,
    release: PathBuf,
}

pub async fn stage(options: AptOptions) -> Result<(), Whatever> {
    info!(suite = %options.suite, "starting apt repository stage");
    validate_options(&options)?;
    ensure_apt_ftparchive().await?;

    let target_dir = target_dir()?;
    let paths = common_paths()?;
    let debs = discover_debs(&target_dir).await?;
    let version = release_version(&debs)?;
    let manifest = read_existing_manifest(&paths.manifest).await?;
    let staging = paths.root.join("apt.staging");
    recreate_dir(&staging).await?;

    let mut artifact_entries = Vec::new();
    for deb in debs {
        let relative = pool_path(&deb.package, &deb.filename);
        let destination = staging.join(&relative);
        copy_artifact(&deb.source, &destination).await?;
        artifact_entries.push(artifact_entry(&staging, &destination, true).await?);
        info!(path = %destination.display(), "staged deb package");
    }

    let mut metadata_files = generate_binary_metadata(&staging, &options).await?;
    let release = generate_suite_release(&staging, &options.suite).await?;
    metadata_files.push(release.clone());
    let signed = sign_suite_release(&staging, &options).await?;
    metadata_files.extend(signed);

    for metadata_file in metadata_files {
        artifact_entries.push(artifact_entry(&staging, &metadata_file, false).await?);
    }

    let manifest = merge_apt_manifest(manifest, &version, artifact_entries);
    let manifest_staging = paths.root.join("manifest.toml.staging");
    write_manifest(&manifest_staging, &manifest).await?;

    promote_staged_outputs(
        "apt",
        &staging,
        &paths.apt,
        &manifest_staging,
        &paths.manifest,
    )
    .await?;

    info!(path = %paths.apt.display(), "finished apt repository stage");
    Ok(())
}

fn validate_options(options: &AptOptions) -> Result<(), Whatever> {
    validate_path_segment("suite", &options.suite)?;
    snafu::ensure_whatever!(
        !options.components.is_empty(),
        "at least one apt component is required"
    );
    for component in &options.components {
        validate_path_segment("component", component)?;
    }
    Ok(())
}

fn validate_path_segment(kind: &str, value: &str) -> Result<(), Whatever> {
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

fn pool_path(package: &str, filename: &str) -> PathBuf {
    let first = package.chars().next().unwrap_or('_');
    PathBuf::from("pool")
        .join("main")
        .join(first.to_string())
        .join(package)
        .join(filename)
}

async fn discover_debs(target_dir: &Path) -> Result<Vec<DebSource>, Whatever> {
    let mut debs = Vec::new();
    for relative in DEB_SEARCH_DIRS {
        let directory = target_dir.join(relative);
        if !tokio::fs::try_exists(&directory)
            .await
            .whatever_context(format!("failed to inspect {}", directory.display()))?
        {
            info!(path = %directory.display(), "skipping missing deb directory");
            continue;
        }

        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .whatever_context(format!("failed to read {}", directory.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .whatever_context(format!("failed to read entry in {}", directory.display()))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .whatever_context(format!("failed to inspect {}", path.display()))?;
            if !file_type.is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("deb")
            {
                continue;
            }
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .whatever_context("failed to read deb filename as utf-8")?
                .to_string();
            let (binary_package, version) = parse_deb_filename(&filename)?;
            let package = source_package(&path).await?.unwrap_or(binary_package);
            debs.push(DebSource {
                package,
                version,
                filename,
                source: path,
            });
        }
    }

    snafu::ensure_whatever!(
        !debs.is_empty(),
        "no deb packages found in target directories"
    );
    Ok(debs)
}

async fn source_package(path: &Path) -> Result<Option<String>, Whatever> {
    let output = tokio::process::Command::new("dpkg-deb")
        .arg("-f")
        .arg(path)
        .arg("Source")
        .output()
        .await;
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).whatever_context(format!(
                "failed to inspect source package for {}",
                path.display()
            ));
        }
    };
    let stdout = String::from_utf8(output.stdout).whatever_context(format!(
        "failed to decode source package for {}",
        path.display()
    ))?;
    let source = stdout
        .split_whitespace()
        .next()
        .filter(|source| !source.is_empty())
        .map(str::to_string);
    Ok(source)
}

fn parse_deb_filename(filename: &str) -> Result<(String, String), Whatever> {
    let (package, rest) = filename
        .split_once('_')
        .whatever_context(format!("failed to infer package name from {filename}"))?;
    let (version_revision, _) = rest
        .split_once('_')
        .whatever_context(format!("failed to infer package version from {filename}"))?;
    let version = version_revision
        .rsplit_once('-')
        .map(|(version, _)| version)
        .unwrap_or(version_revision);
    Ok((package.to_string(), version.to_string()))
}

fn release_version(debs: &[DebSource]) -> Result<String, Whatever> {
    let version = debs
        .first()
        .whatever_context("no deb packages found in target directories")?
        .version
        .clone();
    snafu::ensure_whatever!(
        debs.iter().all(|deb| deb.version == version),
        "deb packages contain multiple release versions"
    );
    Ok(version)
}

async fn generate_binary_metadata(
    repository: &Path,
    options: &AptOptions,
) -> Result<Vec<PathBuf>, Whatever> {
    let mut metadata_files = Vec::new();
    for component in &options.components {
        for arch in APT_ARCHES {
            let paths = binary_metadata_paths(&options.suite, component, arch);
            let directory = repository.join(
                paths
                    .packages
                    .parent()
                    .whatever_context("binary package path must have a parent")?,
            );
            tokio::fs::create_dir_all(&directory)
                .await
                .whatever_context(format!("failed to create {}", directory.display()))?;
            let packages = repository.join(&paths.packages);
            if component == "main" {
                scan_packages(repository, arch, &packages).await?;
            } else {
                tokio::fs::write(&packages, "")
                    .await
                    .whatever_context(format!("failed to write {}", packages.display()))?;
            }

            let packages_gz = repository.join(&paths.packages_gz);
            gzip_file(&packages, &packages_gz).await?;
            let release = repository.join(&paths.release);
            write_binary_release(&release, &options.suite, component, arch).await?;
            metadata_files.extend([packages, packages_gz, release]);
        }
    }
    Ok(metadata_files)
}

fn binary_metadata_paths(suite: &str, component: &str, arch: &str) -> BinaryMetadataPaths {
    let base = PathBuf::from("dists")
        .join(suite)
        .join(component)
        .join(format!("binary-{arch}"));
    BinaryMetadataPaths {
        packages: base.join("Packages"),
        packages_gz: base.join("Packages.gz"),
        release: base.join("Release"),
    }
}

async fn scan_packages(repository: &Path, arch: &str, packages: &Path) -> Result<(), Whatever> {
    let output = std::fs::File::create(packages)
        .whatever_context(format!("failed to create {}", packages.display()))?;
    run_cmd(
        tokio::process::Command::new("dpkg-scanpackages")
            .current_dir(repository)
            .args(["--arch", arch, "pool", "/dev/null"])
            .stdout(Stdio::from(output)),
    )
    .await
    .whatever_context(format!("failed to generate {}", packages.display()))
}

async fn gzip_file(source: &Path, destination: &Path) -> Result<(), Whatever> {
    let source = source.to_owned();
    let destination = destination.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut input = std::fs::File::open(&source)
            .whatever_context(format!("failed to open {}", source.display()))?;
        let output = std::fs::File::create(&destination)
            .whatever_context(format!("failed to create {}", destination.display()))?;
        let mut encoder = GzEncoder::new(output, Compression::default());
        std::io::copy(&mut input, &mut encoder)
            .whatever_context(format!("failed to compress {}", source.display()))?;
        encoder
            .finish()
            .whatever_context(format!("failed to finish {}", destination.display()))?;
        Ok(())
    })
    .await
    .whatever_context("gzip task panicked")?
}

async fn write_binary_release(
    path: &Path,
    suite: &str,
    component: &str,
    arch: &str,
) -> Result<(), Whatever> {
    let content = format!("Archive: {suite}\nComponent: {component}\nArchitecture: {arch}\n");
    tokio::fs::write(path, content)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}

async fn generate_suite_release(repository: &Path, suite: &str) -> Result<PathBuf, Whatever> {
    let release = repository.join("dists").join(suite).join("Release");
    ensure_apt_ftparchive().await?;
    let output = std::fs::File::create(&release)
        .whatever_context(format!("failed to create {}", release.display()))?;
    run_cmd(
        tokio::process::Command::new("apt-ftparchive")
            .current_dir(repository)
            .args(["release", &format!("dists/{suite}")])
            .stdout(Stdio::from(output)),
    )
    .await
    .whatever_context(format!("failed to generate {}", release.display()))?;
    Ok(release)
}

async fn ensure_apt_ftparchive() -> Result<(), Whatever> {
    let status = tokio::process::Command::new("which")
        .arg("apt-ftparchive")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .whatever_context("failed to spawn which")?;
    snafu::ensure_whatever!(
        status.success(),
        "apt-ftparchive is required to stage apt metadata; install apt-utils"
    );
    Ok(())
}

async fn sign_suite_release(
    repository: &Path,
    options: &AptOptions,
) -> Result<Vec<PathBuf>, Whatever> {
    let homedir = tempfile::tempdir().whatever_context("failed to create temporary gpg home")?;
    import_key(&homedir, &options.key_file).await?;
    verify_fingerprint(&homedir, &options.fingerprint).await?;

    let release = repository
        .join("dists")
        .join(&options.suite)
        .join("Release");
    let release_gpg = repository
        .join("dists")
        .join(&options.suite)
        .join("Release.gpg");
    let in_release = repository
        .join("dists")
        .join(&options.suite)
        .join("InRelease");

    let mut detach = base_gpg_sign_command(&homedir, options);
    detach
        .args(["--detach-sign", "--armor", "-o"])
        .arg(&release_gpg)
        .arg(&release);
    run_cmd(&mut detach)
        .await
        .whatever_context(format!("failed to sign {}", release.display()))?;

    let mut clearsign = base_gpg_sign_command(&homedir, options);
    clearsign
        .args(["--clearsign", "-o"])
        .arg(&in_release)
        .arg(&release);
    run_cmd(&mut clearsign)
        .await
        .whatever_context(format!("failed to clearsign {}", release.display()))?;

    Ok(vec![release_gpg, in_release])
}

async fn import_key(homedir: &TempDir, key_file: &Path) -> Result<(), Whatever> {
    run_cmd(
        tokio::process::Command::new("gpg")
            .arg("--batch")
            .arg("--homedir")
            .arg(homedir.path())
            .arg("--import")
            .arg(key_file),
    )
    .await
    .whatever_context(format!(
        "failed to import gpg key from {}",
        key_file.display()
    ))
}

async fn verify_fingerprint(homedir: &TempDir, fingerprint: &str) -> Result<(), Whatever> {
    let output = tokio::process::Command::new("gpg")
        .arg("--batch")
        .arg("--homedir")
        .arg(homedir.path())
        .arg("--with-colons")
        .arg("--fingerprint")
        .arg(fingerprint)
        .output()
        .await
        .whatever_context("failed to run gpg fingerprint verification")?;
    snafu::ensure_whatever!(
        output.status.success(),
        "gpg fingerprint verification failed"
    );
    let stdout = String::from_utf8(output.stdout)
        .whatever_context("failed to decode gpg fingerprint output")?;
    let expected = normalize_fingerprint(fingerprint);
    let matched = stdout.lines().any(|line| {
        let mut fields = line.split(':');
        if fields.next() != Some("fpr") {
            return false;
        }
        fields
            .nth(8)
            .map(normalize_fingerprint)
            .is_some_and(|actual| fingerprint_matches(&actual, &expected))
    });
    snafu::ensure_whatever!(matched, "gpg fingerprint did not match imported key");
    Ok(())
}

fn fingerprint_matches(actual: &str, expected: &str) -> bool {
    !expected.is_empty() && actual == expected
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

fn base_gpg_sign_command(homedir: &TempDir, options: &AptOptions) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("gpg");
    command
        .arg("--batch")
        .arg("--yes")
        .arg("--homedir")
        .arg(homedir.path())
        .arg("--pinentry-mode")
        .arg("loopback")
        .arg("--default-key")
        .arg(&options.fingerprint);
    if let Some(passphrase_file) = &options.passphrase_file {
        command.arg("--passphrase-file").arg(passphrase_file);
    }
    command
}

async fn artifact_entry(
    root: &Path,
    file: &Path,
    immutable: bool,
) -> Result<ArtifactEntry, Whatever> {
    Ok(ArtifactEntry {
        root: ArtifactRoot::Apt,
        path: relative_path(root, file)?,
        sha256: sha256_file(file).await?,
        immutable,
    })
}

async fn read_existing_manifest(path: &Path) -> Result<ReleaseManifest, Whatever> {
    if tokio::fs::try_exists(path)
        .await
        .whatever_context(format!("failed to inspect {}", path.display()))?
    {
        read_manifest(path).await
    } else {
        Ok(ReleaseManifest {
            schema_version: 1,
            package: PACKAGE_NAME.to_string(),
            version: String::new(),
            artifacts: Vec::new(),
        })
    }
}

fn merge_apt_manifest(
    mut manifest: ReleaseManifest,
    version: &str,
    artifacts: Vec<ArtifactEntry>,
) -> ReleaseManifest {
    manifest.package = PACKAGE_NAME.to_string();
    manifest.version = version.to_string();
    manifest
        .artifacts
        .retain(|artifact| artifact.root != ArtifactRoot::Apt);
    manifest.artifacts.extend(artifacts);
    manifest
}

#[cfg(test)]
mod tests {
    use super::{
        binary_metadata_paths, fingerprint_matches, normalize_fingerprint, parse_deb_filename,
        pool_path, validate_path_segment,
    };

    #[test]
    fn pool_path_uses_debian_pool_layout() {
        assert_eq!(
            pool_path("pishoo", "pishoo_0.5.1-1_amd64.deb"),
            std::path::PathBuf::from("pool/main/p/pishoo/pishoo_0.5.1-1_amd64.deb")
        );
    }

    #[test]
    fn common_package_pool_path_uses_source_package() {
        assert_eq!(
            pool_path("pishoo", "pishoo-common_0.5.1-1_all.deb"),
            std::path::PathBuf::from("pool/main/p/pishoo/pishoo-common_0.5.1-1_all.deb")
        );
    }

    #[test]
    fn binary_metadata_paths_use_apt_layout() {
        let paths = binary_metadata_paths("stable", "main", "amd64");

        assert_eq!(
            paths.packages,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Packages")
        );
        assert_eq!(
            paths.packages_gz,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Packages.gz")
        );
        assert_eq!(
            paths.release,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Release")
        );
    }

    #[test]
    fn deb_filename_parser_extracts_package_and_upstream_version() {
        let (package, version) =
            parse_deb_filename("pishoo_0.5.1-1_amd64.deb").expect("filename should parse");

        assert_eq!(package, "pishoo");
        assert_eq!(version, "0.5.1");
    }

    #[test]
    fn path_segment_rejects_path_traversal() {
        let error = validate_path_segment("suite", "../evil")
            .expect_err("path segment with slash should fail");

        assert!(
            error
                .to_string()
                .starts_with("suite must be a single path segment")
        );
    }

    #[test]
    fn fingerprint_normalization_removes_spaces_and_uppercases() {
        assert_eq!(normalize_fingerprint("ab cd ef"), "ABCDEF");
    }

    #[test]
    fn fingerprint_matching_requires_full_fingerprint() {
        assert!(fingerprint_matches(
            "00112233445566778899AABBCCDDEEFF00112233",
            "00112233445566778899AABBCCDDEEFF00112233"
        ));
        assert!(!fingerprint_matches(
            "00112233445566778899AABBCCDDEEFF00112233",
            "CCDDEEFF00112233"
        ));
    }
}
