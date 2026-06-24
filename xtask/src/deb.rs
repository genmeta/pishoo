use std::collections::BTreeMap;

use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    },
};
use futures_util::StreamExt;
use snafu::{Report, ResultExt, Whatever};
use tracing::{Instrument, info, info_span};

use crate::{
    BuildProfile, DebTarget, Feature,
    container::{
        CARGO_HOME, ContainerSourceLayout, RUSTUP_HOME, ZIG_GLIBC_VERSION, cargo_cache_mounts,
        cargo_config_from_siblings, check_docker, dhttp_bootstrap_from_values, exec_in_container,
        force_remove_container, host_uid_gid, install_cargo_config, remove_container_if_exists,
        source_layout, source_mounts, start_container,
    },
    package_version,
    release_contract::{PackageKind, ReleaseContract, resolve_build_env_from_process},
    target_dir,
};

const CARGO_NAME: &str = "pishoo";

/// Install directory for pishoo helper binaries (FHS 3.0 / Debian Policy 4.6.1).
/// Shared with rpm packaging so both formats ship helpers to the same path.
pub(crate) const PISHOO_LIBEXEC_DIR: &str = "/usr/libexec/pishoo";

/// Base Docker image for cross-compilation.
const BASE_IMAGE: &str = "debian:bookworm";

/// Image tag prefix for pishoo deb builds.
const IMAGE_TAG_PREFIX: &str = "pishoo-deb-v2";
const BUILD_ATTEMPTS: usize = 2;

/// Relative path from workspace root to the debian packaging source files.
const DEBIAN_PKG_DIR: &str = "xtask/deb";
const DEBIAN_CONTROL_TEMPLATE: &str = include_str!("../deb/control");
const PISHOO_COMMON_DEPENDS_PLACEHOLDER: &str = "{{PISHOO_COMMON_DEPENDS}}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebArtifact {
    pub target: String,
    pub path: std::path::PathBuf,
    pub features: Vec<String>,
    pub profile: BuildProfile,
}

#[derive(Clone, Copy)]
struct BinaryBuildContext<'a> {
    target_dir: &'a std::path::Path,
    profile: BuildProfile,
    features: &'a [Feature],
    layout: &'a ContainerSourceLayout,
    binary_control: &'a str,
    build_env: &'a BTreeMap<String, String>,
}

fn binary_package_version(version: &str) -> String {
    format!("{version}-1")
}

fn render_binary_control(required_common_version: &str, binary_package_version: &str) -> String {
    let common_depends = format!(
        "pishoo-common (>= {required_common_version}), pishoo-common (<= {binary_package_version})"
    );
    DEBIAN_CONTROL_TEMPLATE.replace(PISHOO_COMMON_DEPENDS_PLACEHOLDER, &common_depends)
}

fn deb_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("amd64"),
        "aarch64-unknown-linux-gnu" => Ok("arm64"),
        "armv7-unknown-linux-gnueabihf" => Ok("armhf"),
        "i686-unknown-linux-gnu" => Ok("i386"),
        _ => snafu::whatever!("unsupported deb target triple: {triple}"),
    }
}

/// GNU architecture prefix used for cross-compilation lib paths.
fn gnu_arch(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64-linux-gnu"),
        "armv7-unknown-linux-gnueabihf" => Ok("arm-linux-gnueabihf"),
        "i686-unknown-linux-gnu" => Ok("i386-linux-gnu"),
        _ => snafu::whatever!("unsupported gnu arch for triple: {triple}"),
    }
}

// (shared docker/container helpers live in crate::container)

/// Ensure the Debian base image is available locally.
///
/// Docker's pull API contacts the registry even when the tag already exists
/// locally. Packaging should only depend on the registry when the local image is
/// missing; otherwise a transient registry timeout can fail an otherwise
/// reproducible local build.
async fn ensure_base_image(docker: &Docker) -> Result<(), Whatever> {
    if docker.inspect_image(BASE_IMAGE).await.is_ok() {
        info!(image = BASE_IMAGE, "base image already exists");
        return Ok(());
    }

    let mut pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(BASE_IMAGE)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        result.whatever_context(format!("failed to pull base image {BASE_IMAGE}"))?;
    }

    Ok(())
}

/// Ensure the build image exists for the given target triple.
/// Installs cross-compilation toolchain, libc-dev, and libpam0g-dev.
async fn ensure_image(docker: &Docker, triple: &str) -> Result<String, Whatever> {
    let deb = deb_arch(triple)?;
    let tag = format!("xtask-{triple}:{IMAGE_TAG_PREFIX}");

    if docker.inspect_image(&tag).await.is_ok() {
        info!(tag, "image already exists");
        return Ok(tag);
    }

    info!(tag, "building image");

    ensure_base_image(docker).await?;

    // Create temp container from base
    let container_name = format!("{CARGO_NAME}-xtask-setup-{triple}");
    // Remove a leaked container left by a previous failed run so we can retry.
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create setup container")?;
    let container_id = container.id.clone();

    // Any failure past this point must still remove the container, otherwise
    // the name collides on retry.
    let setup_result = ensure_image_inner(docker, &container_id, triple, deb).await;

    if setup_result.is_err() {
        force_remove_container(docker, &container_id).await;
        setup_result?;
        unreachable!();
    }

    // Commit
    let repo = tag.split(':').next().unwrap_or(&tag);
    let img_tag = tag.split(':').nth(1).unwrap_or(IMAGE_TAG_PREFIX);
    let commit_result = docker
        .commit_container(
            CommitContainerOptionsBuilder::default()
                .container(&container_id)
                .repo(repo)
                .tag(img_tag)
                .build(),
            ContainerConfig::default(),
        )
        .await
        .whatever_context("failed to commit image");

    // Always remove the setup container before returning.
    force_remove_container(docker, &container_id).await;
    commit_result?;

    info!(tag, "image ready");
    Ok(tag)
}

/// Run the toolchain-installation steps inside an already-created container.
async fn ensure_image_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    deb: &str,
) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;

    // Install Rust toolchain, Zig, cargo-zigbuild, and cross-compilation libs.
    // Toolchain is installed to /opt/cargo + /opt/rustup so any uid can use it.
    let setup_script = format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --assume-yes -qq \
    ca-certificates curl build-essential pkg-config libclang-dev wget

# install rust into globally readable paths
export CARGO_HOME={CARGO_HOME}
export RUSTUP_HOME={RUSTUP_HOME}
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- --default-toolchain nightly --profile minimal -y
export PATH="{CARGO_HOME}/bin:$PATH"
rustup target add {triple}

# install zig
wget -q https://ziglang.org/download/0.14.0/zig-linux-x86_64-0.14.0.tar.xz
tar -xf zig-linux-x86_64-0.14.0.tar.xz
mv zig-linux-x86_64-0.14.0 /usr/local/zig
ln -s /usr/local/zig/zig /usr/local/bin/zig
rm zig-linux-x86_64-0.14.0.tar.xz

cargo install cargo-zigbuild

# cross-compilation libraries
dpkg --add-architecture {deb}
apt-get update -qq
apt-get install --assume-yes -qq libc-dev:{deb} libpam0g-dev:{deb} dpkg-dev debhelper fakeroot

# make toolchain readable by any user
chmod -R a+rX {CARGO_HOME} {RUSTUP_HOME}
"#
    );
    exec_in_container(docker, container_id, &["bash", "-c", &setup_script], None).await?;
    Ok(())
}

/// Build the arch-independent pishoo-common config package.
async fn run_common(
    docker: &Docker,
    common_package_version: &str,
    binary_control: &str,
    layout: &ContainerSourceLayout,
) -> Result<DebArtifact, Whatever> {
    info!("starting common deb package build");
    let target_dir = target_dir()?;
    let out_dir = target_dir.join("common").join("deb");
    tokio::fs::create_dir_all(&out_dir)
        .await
        .whatever_context(format!("failed to create {}", out_dir.display()))?;

    // pishoo-common only needs debhelper + dpkg-deb in the base image.
    ensure_base_image(docker).await?;

    let container_name = format!("{CARGO_NAME}-xtask-deb-common");
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                host_config: Some(HostConfig {
                    mounts: Some(source_mounts(layout)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create common container")?;
    let container_id = container.id.clone();

    // Always remove the common container before returning, success or not.
    let result = run_common_inner(
        docker,
        &container_id,
        common_package_version,
        binary_control,
        &layout.primary.container,
    )
    .await;
    force_remove_container(docker, &container_id).await;
    result?;

    let deb_name = format!("pishoo-common_{common_package_version}_all.deb");
    info!(deb_name, "produced");
    info!("finished common deb package build");
    Ok(DebArtifact {
        target: "common".to_string(),
        path: out_dir.join(deb_name),
        features: Vec::new(),
        profile: BuildProfile::Release,
    })
}

async fn run_common_inner(
    docker: &Docker,
    container_id: &str,
    common_package_version: &str,
    binary_control: &str,
    primary_source: &str,
) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;

    // Install debhelper inside the base container (no cross-compilation image needed)
    let setup_script = r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --assume-yes -qq debhelper fakeroot
"#;
    exec_in_container(docker, container_id, &["bash", "-c", setup_script], None).await?;

    // Prepare debian source tree under target/common/deb/src/ and run dpkg-buildpackage.
    // Products (.deb etc.) land in target/common/deb/ (one level above src/).
    // Runs as host uid:gid so files in target/ are owned by the host user.
    let user = host_uid_gid()?;
    let build_script =
        render_common_build_script(common_package_version, binary_control, primary_source);
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &build_script],
        Some(&user),
    )
    .await?;

    Ok(())
}

fn render_common_build_script(
    common_package_version: &str,
    binary_control: &str,
    primary_source: &str,
) -> String {
    let control_escaped = shell_escape(binary_control);
    format!(
        r#"set -e
export SOURCE_ROOT={primary_source}
SRC={primary_source}/target/common/deb/src
mkdir -p "$SRC/debian"
cp -r {primary_source}/{DEBIAN_PKG_DIR}/. "$SRC/debian/"
printf '%s' {control_escaped} > "$SRC/debian/control"
printf '{CARGO_NAME} ({common_package_version}) unstable; urgency=low\n\n  * release {common_package_version}\n\n -- Genmeta Tech Limited <developer@genmeta.net>  %s\n' \
    "$(date -R)" > "$SRC/debian/changelog"
cd "$SRC"
dpkg-buildpackage -A -uc -us -d
"#
    )
}

pub async fn run(
    contract: &ReleaseContract,
    targets: &[DebTarget],
    profile: BuildProfile,
    features: &[Feature],
    siblings: &[std::path::PathBuf],
) -> Result<Vec<DebArtifact>, Whatever> {
    info!(
        target_count = targets.len(),
        profile = profile.target_dir_name(),
        "starting deb dist build"
    );
    let docker = Docker::connect_with_local_defaults()
        .whatever_context("failed to connect to Docker/Podman")?;
    check_docker(&docker).await?;

    // Resolve source paths up front so every target build sees the same set
    // and path errors surface before we spin up containers.
    let layout = source_layout("gateway", siblings)?;

    let version = package_version(CARGO_NAME)?;
    let binary_package_version = binary_package_version(&version);
    let binary_control = render_binary_control(
        &contract.package.common.required_version,
        &binary_package_version,
    );
    let target_dir = target_dir()?;
    let build_env = resolve_build_env_from_process(contract, PackageKind::Deb, None)
        .whatever_context("failed to resolve build environment for deb packaging")?;

    let mut tasks = tokio::task::JoinSet::new();

    for &target in targets {
        if matches!(target, DebTarget::Common) {
            let docker = docker.clone();
            let common_package_version = contract.package.common.version.clone();
            let binary_control = binary_control.clone();
            let layout = layout.clone();
            info!("queued common deb package build");
            tasks.spawn(
                async move {
                    run_common(&docker, &common_package_version, &binary_control, &layout).await
                }
                .instrument(info_span!("deb", triple = "common")),
            );
            continue;
        }

        let docker = docker.clone();
        let triple = target.triple();
        info!(triple, "queued deb target build");
        let span = info_span!("deb", triple);
        let version = version.clone();
        let binary_control = binary_control.clone();
        let target_dir = target_dir.clone();
        let features = features.to_vec();
        let layout = layout.clone();
        let build_env = build_env.clone();
        tasks.spawn(
            async move {
                build_one_with_retry(
                    &docker,
                    triple,
                    &version,
                    BinaryBuildContext {
                        target_dir: &target_dir,
                        profile,
                        features: &features,
                        layout: &layout,
                        binary_control: &binary_control,
                        build_env: &build_env,
                    },
                )
                .await
            }
            .instrument(span),
        );
    }

    let mut artifacts = Vec::new();
    while let Some(result) = tasks.join_next().await {
        artifacts.push(result.whatever_context("deb build task panicked")??);
    }
    artifacts.sort_by(|left, right| left.target.cmp(&right.target));

    info!("finished deb dist build");

    Ok(artifacts)
}

async fn build_one_with_retry(
    docker: &Docker,
    triple: &str,
    version: &str,
    context: BinaryBuildContext<'_>,
) -> Result<DebArtifact, Whatever> {
    for attempt in 1..=BUILD_ATTEMPTS {
        match build_one(docker, triple, version, context).await {
            Ok(artifact) => return Ok(artifact),
            Err(error) if attempt < BUILD_ATTEMPTS => {
                let report = Report::from_error(&error);
                tracing::warn!(
                    %triple,
                    attempt,
                    error = %report,
                    "deb target build failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("build attempts loop must return")
}

async fn build_one(
    docker: &Docker,
    triple: &str,
    version: &str,
    context: BinaryBuildContext<'_>,
) -> Result<DebArtifact, Whatever> {
    let has_sshd = context
        .features
        .iter()
        .any(|f| matches!(f, Feature::Sshd | Feature::Pam));
    let arch = deb_arch(triple)?;
    let gnu = gnu_arch(triple)?;
    info!(triple, "ensuring build image");
    let image = ensure_image(docker, triple).await?;

    let deb_name = format!("{CARGO_NAME}_{version}-1_{arch}.deb");
    let profile_dir = context.profile.target_dir_name();
    let out_dir = context
        .target_dir
        .join(triple)
        .join(profile_dir)
        .join("deb");
    tokio::fs::create_dir_all(&out_dir)
        .await
        .whatever_context(format!("failed to create {}", out_dir.display()))?;

    let mut mounts = source_mounts(context.layout);
    mounts.extend(cargo_cache_mounts());

    let bootstrap = dhttp_bootstrap_from_values(context.build_env.clone())?;
    mounts.extend(bootstrap.mounts);
    let cargo_config = cargo_config_from_siblings(&context.layout.overrides);

    let container_name = format!("{CARGO_NAME}-xtask-deb-{triple}");
    info!(triple, container = %container_name, "creating build container");
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(image.clone()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                host_config: Some(HostConfig {
                    mounts: Some(mounts),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create build container")?;
    let container_id = container.id.clone();

    // Build cargo features string
    let cargo_features = {
        let names: Vec<&str> = context
            .features
            .iter()
            .map(|f| match f {
                Feature::Sshd => "sshd",
                Feature::Pam => "pam",
            })
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            names.join(",")
        }
    };

    // Environment variables for debian/rules override_dh_auto_build.
    // pishoo-worker is always required; pishoo-ssh-session only when sshd feature is enabled.
    // Paths follow FHS /usr/libexec convention (shared with rpm packaging).
    let worker_env = format!("export PISHOO_WORKER_BIN={PISHOO_LIBEXEC_DIR}/pishoo-worker && ");
    let ssh_session_env = if has_sshd {
        format!("export PISHOO_SSH_SESSION_BIN={PISHOO_LIBEXEC_DIR}/pishoo-ssh-session && ")
    } else {
        String::new()
    };

    let cargo_features_env = if cargo_features.is_empty() {
        String::new()
    } else {
        format!("export CARGO_FEATURES={cargo_features} && ")
    };

    // Run the actual build; always clean up the container regardless of outcome.
    let result = build_one_inner(
        docker,
        &container_id,
        triple,
        version,
        arch,
        gnu,
        context.profile,
        &context.layout.primary.container,
        &bootstrap.exports,
        cargo_config.as_deref(),
        &worker_env,
        &ssh_session_env,
        &cargo_features_env,
        context.binary_control,
    )
    .await;
    force_remove_container(docker, &container_id).await;
    result?;

    info!(deb_name, "produced");
    Ok(DebArtifact {
        target: triple.to_string(),
        path: out_dir.join(deb_name),
        features: crate::brew::feature_names(context.features),
        profile: context.profile,
    })
}

#[allow(clippy::too_many_arguments)]
async fn build_one_inner(
    docker: &Docker,
    container_id: &str,
    triple: &str,
    version: &str,
    arch: &str,
    gnu: &str,
    profile: BuildProfile,
    primary_source: &str,
    dhttp_bootstrap_exports: &str,
    cargo_config: Option<&str>,
    worker_env: &str,
    ssh_session_env: &str,
    cargo_features_env: &str,
    binary_control: &str,
) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;
    info!(triple, "build container started");

    // Install cross-compilation binutils (needs root).
    let install_binutils = format!("apt-get install -y -qq binutils-{gnu} 2>/dev/null || true");
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &install_binutils],
        None,
    )
    .await?;
    install_cargo_config(docker, container_id, cargo_config).await?;

    // dpkg-buildpackage -B builds only Architecture: any packages (pishoo binary).
    // -a{arch} sets the host architecture for cross-compilation.
    // Prepare debian source tree under target/{triple}/{profile}/deb/src/ so that
    // all temp files and products stay inside target/ (bind-mounted, gitignored).
    // Runs as host uid:gid so files in target/ are owned by the host user.
    let user = host_uid_gid()?;
    let profile_dir = profile.target_dir_name();
    let cargo_profile_args = profile.cargo_profile_args().join(" ");
    let control_escaped = shell_escape(binary_control);
    let build_script = format!(
        r#"set -e
export HOME=/tmp
export PATH="{CARGO_HOME}/bin:/usr/local/zig:$PATH"
export RUSTUP_HOME={RUSTUP_HOME}
export CARGO_HOME={CARGO_HOME}
export TRIPLE={triple}
export ZIG_TARGET={triple}.{ZIG_GLIBC_VERSION}
export BUILD_PROFILE={profile_dir}
export CARGO_PROFILE_ARGS="{cargo_profile_args}"
export DEB_HOST_MULTIARCH={gnu}
export SOURCE_ROOT={primary_source}
{dhttp_bootstrap_exports}{worker_env}{ssh_session_env}{cargo_features_env}
SRC={primary_source}/target/{triple}/{profile_dir}/deb/src
mkdir -p "$SRC/debian"
cp -r {primary_source}/{DEBIAN_PKG_DIR}/. "$SRC/debian/"
printf '%s' {control_escaped} > "$SRC/debian/control"
printf '{CARGO_NAME} ({version}-1) unstable; urgency=low\n\n  * release {version}\n\n -- Genmeta Tech Limited <developer@genmeta.net>  %s\n' \
    "$(date -R)" > "$SRC/debian/changelog"
cd "$SRC"
dpkg-buildpackage -B -uc -us -d -a{arch}
"#
    );

    info!(triple, "starting dpkg-buildpackage inside container");
    exec_in_container(
        docker,
        container_id,
        &["bash", "-c", &build_script],
        Some(&user),
    )
    .await?;
    info!(triple, "dpkg-buildpackage finished inside container");

    Ok(())
}

fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::{DEBIAN_PKG_DIR, render_binary_control, render_common_build_script};

    #[test]
    fn binary_control_uses_common_dependency_range() {
        let control = render_binary_control("0.5.1-1", "0.5.2-1");

        assert!(control.contains(
            "Depends: pishoo-common (>= 0.5.1-1), pishoo-common (<= 0.5.2-1), ${shlibs:Depends}, ${misc:Depends}\n"
        ));
    }

    #[test]
    fn common_build_script_exports_primary_source_root() {
        let script =
            render_common_build_script("0.5.2-1", "Package: pishoo-common\n", "/sources/gateway");

        assert!(script.contains("export SOURCE_ROOT=/sources/gateway\n"));
        assert!(script.contains("SRC=/sources/gateway/target/common/deb/src\n"));
        assert!(!script.contains("/workspace/xtask/deb/common"));
    }

    #[test]
    fn common_postinst_creates_group_and_dhttp_home() {
        let postinst = include_str!("../deb/pishoo-common.postinst");

        assert!(postinst.contains("addgroup --system --quiet pishoo || true"));
        assert!(postinst.contains("install -d -m 0755 /etc/dhttp"));
        assert_eq!(DEBIAN_PKG_DIR, "xtask/deb");
    }
}
